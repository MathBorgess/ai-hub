//! Transporte remoto: listener WebSocket em loopback com canal de controle (JSON) e canal de
//! PTY (binário) desacoplados (01-transporte-e-sessao.md §2.1, §2.2; ADR §2.2, contradição 1).
//!
//! O framing WebSocket em si é `crate::ws_proto` (escrito à mão — ver esse módulo para o
//! porquê). Este módulo decide, por conexão aceita, se ela é controle ou PTY (pela forma da
//! primeira mensagem), e implementa cada papel:
//! - **Controle:** reautentica por prova de posse (como `run_tcp`), então despacha via
//!   `Daemon::dispatch` — o mesmo caminho do socket local. Heartbeat de 5s/15s vive aqui.
//! - **PTY:** exige um `PtyChannelHello` com `channel_ticket` válido antes de ver qualquer
//!   byte da sessão; depois disso é um pipe binário puro, sem JSON.
use crate::auth::{self, PrincipalId};
use crate::ws_proto::{self, WsMessage};
use crate::{error, log_lifecycle, Daemon, DaemonMessage, PROTOCOL_VERSION};
use aihub_core::{ClientMessage, IpcMessage, PtyBinaryFrame, PtyChannelHello};
use anyhow::{bail, Result};
use rand::Rng;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::task::JoinSet;

/// Heartbeat cadence on the control channel (01-transporte-e-sessao.md §2.4).
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
/// Liveness timeout: three missed heartbeat windows drop the *transport*, never the session
/// nor its child (§2.4 — this is exactly what makes closing the laptop lid safe).
const LIVENESS_TIMEOUT: Duration = Duration::from_secs(15);

async fn ws_send<W: AsyncWrite + Unpin>(writer: &mut W, msg: &DaemonMessage) -> Result<()> {
    let text = serde_json::to_string(&IpcMessage::Daemon(msg.clone()))?;
    ws_proto::write_message(writer, &WsMessage::Text(text)).await
}

async fn ws_recv_client_message<R: AsyncRead + Unpin>(reader: &mut R) -> Result<ClientMessage> {
    match ws_proto::read_message(reader).await? {
        WsMessage::Text(text) => match serde_json::from_str::<IpcMessage>(&text)? {
            IpcMessage::Client(m) => Ok(m),
            _ => bail!("wrong message direction"),
        },
        other => bail!("expected a text control frame, got {other:?}"),
    }
}

impl Daemon {
    /// WebSocket listener on loopback (ADR §2.5, §6): dual-channel remote transport, running
    /// alongside `run()`'s Unix socket without touching it. Reconciles `sessions.json` once at
    /// startup if `with_sessions_catalog_path` was set (ADR contradição 3, Opção B) — a no-op
    /// otherwise, so tests that never opt in never touch disk.
    pub async fn run_ws(
        &self,
        addr: SocketAddr,
        shutdown: impl std::future::Future<Output = ()>,
    ) -> Result<()> {
        if let Some(path) = &self.catalog_path {
            let reconciled = crate::sessions_catalog::reconcile_startup_catalog(path).await;
            if !reconciled.is_empty() {
                log_lifecycle(
                    "WARN",
                    "orphan_reconciled",
                    &format!(
                        "terminated {} orphaned session(s) from a prior run",
                        reconciled.len()
                    ),
                );
            }
        }
        log_lifecycle(
            "INFO",
            "start_ws",
            &format!("daemon listening (ws) on {addr}"),
        );
        let listener = TcpListener::bind(addr).await?;
        let mut tasks = JoinSet::new();
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                _ = &mut shutdown => break,
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => {
                        let _ = stream.set_nodelay(true);
                        let daemon = self.clone();
                        tasks.spawn(async move { let _ = daemon.ws_connection(stream).await; });
                    }
                    Err(_) => continue,
                },
                Some(_) = tasks.join_next() => {},
            }
        }
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        log_lifecycle(
            "INFO",
            "shutdown_ws",
            "websocket listener shutdown complete",
        );
        Ok(())
    }

    async fn ws_connection(&self, mut stream: TcpStream) -> Result<()> {
        ws_proto::accept_handshake(&mut stream).await?;
        let first = ws_proto::read_message(&mut stream).await?;
        let text = match first {
            WsMessage::Text(t) => t,
            _ => bail!("first websocket frame must be a text handshake message"),
        };
        // The PTY channel's handshake (`PtyChannelHello`) and the control channel's
        // (`ClientMessage::Hello` inside an `IpcMessage`) are distinct, untagged shapes on the
        // wire; try the smaller/more specific one first.
        if let Ok(hello) = serde_json::from_str::<PtyChannelHello>(&text) {
            return self.pty_channel(stream, hello).await;
        }
        match serde_json::from_str::<IpcMessage>(&text) {
            Ok(IpcMessage::Client(hello @ ClientMessage::Hello { .. })) => {
                self.control_channel(stream, hello).await
            }
            _ => bail!("first websocket message was neither PtyChannelHello nor Hello"),
        }
    }

    /// Network control channel: same proof-of-possession handshake as `run_tcp`'s
    /// `authenticate()`, replayed over WS text frames instead of raw framed JSON, then the
    /// exact dispatch path the local Unix socket uses.
    async fn control_channel(&self, stream: TcpStream, hello: ClientMessage) -> Result<()> {
        let (mut reader, writer) = tokio::io::split(stream);
        let writer = Arc::new(Mutex::new(writer));
        let principal = match self.authenticate_ws(&mut reader, &writer, hello).await? {
            Some(p) => p,
            None => return Ok(()),
        };
        let (id, mut rx) = self.register_client(principal.clone()).await?;

        let mut tasks = JoinSet::new();
        let pump_writer = writer.clone();
        tasks.spawn(async move {
            let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
            loop {
                tokio::select! {
                    msg = rx.recv() => match msg {
                        Some(m) => ws_send(&mut *pump_writer.lock().await, &m).await?,
                        None => break,
                    },
                    _ = heartbeat.tick() => {
                        ws_proto::write_message(&mut *pump_writer.lock().await, &WsMessage::Ping(vec![])).await?;
                    }
                }
            }
            Ok::<(), anyhow::Error>(())
        });

        loop {
            let next =
                tokio::time::timeout(LIVENESS_TIMEOUT, ws_proto::read_message(&mut reader)).await;
            let message = match next {
                // Liveness timeout (§2.4): drop the transport only. The session, its ring
                // buffer and its child PTY are all untouched — `register_client`/`dispatch`
                // never learn this happened until `clients.remove` below.
                Err(_) => break,
                Ok(Err(_)) => break,
                Ok(Ok(WsMessage::Close)) => break,
                Ok(Ok(WsMessage::Ping(payload))) => {
                    let _ = ws_proto::write_message(
                        &mut *writer.lock().await,
                        &WsMessage::Pong(payload),
                    )
                    .await;
                    continue;
                }
                Ok(Ok(WsMessage::Pong(_))) => continue,
                Ok(Ok(WsMessage::Binary(_))) => break, // control channel is JSON-only
                Ok(Ok(WsMessage::Text(text))) => match serde_json::from_str::<IpcMessage>(&text) {
                    Ok(IpcMessage::Client(m)) => m,
                    _ => break,
                },
            };
            self.dispatch(id, &principal, message).await;
        }
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        self.state.lock().await.clients.remove(&id);
        Ok(())
    }

    /// Network handshake replayed over WS text frames (`run_tcp`'s `authenticate()` twin).
    /// Always the network trust model: fails closed on anything short of verified proof of
    /// possession (ADR §2.3, §6). The opaque `Unauthorized` reply never distinguishes *why*.
    async fn authenticate_ws<R, W>(
        &self,
        reader: &mut R,
        writer: &Arc<Mutex<W>>,
        hello: ClientMessage,
    ) -> Result<Option<PrincipalId>>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let version = match &hello {
            ClientMessage::Hello { version, .. } => *version,
            _ => {
                ws_send(
                    &mut *writer.lock().await,
                    &error("protocol", "expected Hello at connection start"),
                )
                .await?;
                return Ok(None);
            }
        };
        if version != PROTOCOL_VERSION {
            auth::audit_log(
                &self.audit_path,
                "handshake_rejected",
                None,
                &auth::AuthReject::UnsupportedVersion.to_string(),
            );
            ws_send(&mut *writer.lock().await, &DaemonMessage::Unauthorized).await?;
            return Ok(None);
        }
        let mut nonce = [0u8; 32];
        rand::rng().fill_bytes(&mut nonce);
        ws_send(
            &mut *writer.lock().await,
            &DaemonMessage::Challenge {
                nonce: nonce.to_vec().into(),
            },
        )
        .await?;
        let proof = ws_recv_client_message(reader).await?;
        let credential = match proof {
            ClientMessage::Hello {
                credential: Some(c),
                ..
            } => c,
            _ => {
                auth::audit_log(
                    &self.audit_path,
                    "handshake_rejected",
                    None,
                    &auth::AuthReject::MissingCredential.to_string(),
                );
                ws_send(&mut *writer.lock().await, &DaemonMessage::Unauthorized).await?;
                return Ok(None);
            }
        };
        match auth::verify_credential(&credential, &nonce, (self.clock)(), &self.allowlist) {
            Ok(principal) => {
                auth::audit_log(
                    &self.audit_path,
                    "handshake_accepted",
                    Some(&principal),
                    "proof of possession verified (ws)",
                );
                ws_send(
                    &mut *writer.lock().await,
                    &DaemonMessage::Hello {
                        version: PROTOCOL_VERSION,
                    },
                )
                .await?;
                Ok(Some(principal))
            }
            Err(reason) => {
                auth::audit_log(
                    &self.audit_path,
                    "handshake_rejected",
                    None,
                    &reason.to_string(),
                );
                ws_send(&mut *writer.lock().await, &DaemonMessage::Unauthorized).await?;
                Ok(None)
            }
        }
    }

    /// Secondary PTY data channel: no crypto handshake here at all (that's the whole point of
    /// the ticket, ADR contradição 1, Opção B) — just a single-use `channel_ticket` tying this
    /// connection to a session and the principal that already authenticated on the control
    /// channel. Once redeemed, this connection carries nothing but raw bytes: keystrokes in,
    /// `PtyBinaryFrame`s out, zero JSON/Base64 overhead (§2.2, §5).
    async fn pty_channel(&self, stream: TcpStream, hello: PtyChannelHello) -> Result<()> {
        let principal = {
            let mut state = self.state.lock().await;
            state.tickets.redeem(&hello.ticket, &hello.session_id)
        };
        let (reader, writer) = tokio::io::split(stream);
        let Some(_principal) = principal else {
            // No valid ticket, no bytes: reject before the caller ever sees the session's PTY.
            let mut writer = writer;
            let _ = ws_proto::write_message(&mut writer, &WsMessage::Close).await;
            return Ok(());
        };
        let bootstrap = self.pty_channel_bootstrap(&hello.session_id).await;
        let Some(bootstrap) = bootstrap else {
            let mut writer = writer;
            let _ = ws_proto::write_message(&mut writer, &WsMessage::Close).await;
            return Ok(());
        };

        let writer = Arc::new(Mutex::new(writer));
        let mut tasks = JoinSet::new();

        // Catch-up: whatever is still retained, tagged with its real starting offset. Overlap
        // with the first live chunk below is safe by design — the client dedupes by offset
        // (01-transporte-e-sessao.md §3).
        {
            let frame = PtyBinaryFrame::new(bootstrap.head_offset, bootstrap.snapshot);
            let mut w = writer.lock().await;
            ws_proto::write_message(&mut *w, &WsMessage::Binary(frame.encode())).await?;
        }

        let out_writer = writer.clone();
        let mut rx = bootstrap.rx;
        tasks.spawn(async move {
            loop {
                match rx.recv().await {
                    Ok((offset, data)) => {
                        let frame = PtyBinaryFrame::new(offset, data);
                        ws_proto::write_message(
                            &mut *out_writer.lock().await,
                            &WsMessage::Binary(frame.encode()),
                        )
                        .await?;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
            Ok::<(), anyhow::Error>(())
        });

        let try_write = bootstrap.try_write;
        let mut reader = reader;
        loop {
            match ws_proto::read_message(&mut reader).await {
                Ok(WsMessage::Binary(data)) => {
                    if try_write(data).is_err() {
                        break;
                    }
                }
                Ok(WsMessage::Close) => break,
                Ok(WsMessage::Ping(payload)) => {
                    let _ = ws_proto::write_message(
                        &mut *writer.lock().await,
                        &WsMessage::Pong(payload),
                    )
                    .await;
                }
                Ok(WsMessage::Pong(_)) => {}
                Ok(WsMessage::Text(_)) | Err(_) => break,
            }
        }
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        Ok(())
    }
}

/// Everything the PTY channel task needs to bootstrap, gathered under one lock acquisition so
/// the live subscription and the retained snapshot/offset are consistent with each other
/// (`Daemon::pty_channel_bootstrap`, defined in `lib.rs` since it reaches into `Session`'s
/// private fields there).
pub(crate) struct PtyChannelBootstrap {
    pub rx: tokio::sync::broadcast::Receiver<(u64, Vec<u8>)>,
    pub head_offset: u64,
    pub snapshot: Vec<u8>,
    pub try_write: Arc<dyn Fn(Vec<u8>) -> Result<()> + Send + Sync>,
}
