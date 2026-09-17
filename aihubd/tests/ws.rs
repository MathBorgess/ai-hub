//! Fatia 2 (01-transporte-e-sessao.md; ADR §2.2, contradição 1, contradição 3): transporte
//! WebSocket remoto, canal duplo, `channel_ticket`, ring-buffer de retomada, liveness
//! não-destrutivo e catálogo `sessions.json`. Cobre exatamente os itens de rede real do
//! brief da sessão 04 — o handshake criptográfico em si é da sessão 03 (`aihubd/tests/auth.rs`).
use aihub_core::*;
use aihubd::ws_proto::{self, WsMessage};
use aihubd::{Daemon, Pty};
use ed25519_dalek::{Signature, Signer, SigningKey};
use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::broadcast,
};

static DIR_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn test_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ah-ws-{}-{}-{}",
        std::process::id(),
        DIR_COUNTER.fetch_add(1, Ordering::SeqCst),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn random_signing_key() -> SigningKey {
    use rand::Rng;
    let mut seed = [0u8; 32];
    rand::rng().fill_bytes(&mut seed);
    SigningKey::from_bytes(&seed)
}

fn credential_for(key: &SigningKey, nonce: &[u8], timestamp: u64) -> ClientCredential {
    let signature: Signature = key.sign(nonce);
    ClientCredential {
        public_key: Base64Bytes::new(key.verifying_key().to_bytes().to_vec()),
        signature: Base64Bytes::new(signature.to_bytes().to_vec()),
        audience: CREDENTIAL_AUDIENCE.into(),
        timestamp,
    }
}

fn loopback_addr() -> SocketAddr {
    "127.0.0.1:0".parse().unwrap()
}

/// Mirrors `aihubd/tests/auth.rs`'s `spawn_network_daemon`, but for `run_ws`.
async fn spawn_ws_daemon(daemon: Daemon) -> (SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let listener = TcpListener::bind(loopback_addr()).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        drop(listener);
        daemon
            .run_ws(addr, async {
                stop_rx.await.ok();
            })
            .await
            .unwrap();
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if TcpStream::connect(addr).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("daemon must bind ws listener within 3s");
    (addr, stop_tx)
}

fn dummy_pty_with(
    output: broadcast::Sender<Vec<u8>>,
    scrollback: Vec<u8>,
    write_calls: Arc<AtomicUsize>,
) -> Pty {
    Pty {
        output: output.subscribe(),
        scrollback,
        write: Box::new(|_| Box::pin(async { Ok(()) })),
        resize: Box::new(|_| Ok(())),
        wait: Box::new(move || {
            let _keep = output.clone();
            Box::pin(std::future::pending())
        }),
        kill: Box::new(|| Box::pin(async { Ok(()) })),
        stop: Arc::new(|_| Box::pin(async { Ok(Some(0)) })),
        try_write: Arc::new(move |_| {
            write_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }),
    }
}

/// Raw client-side WS handshake (RFC 6455): reuses the daemon's own hand-rolled `ws_proto`
/// framing for everything after the Upgrade, since neither side depends on `tokio-tungstenite`'s
/// `futures-util`-gated ergonomic API (not declared as a direct dependency in this slice — see
/// `ws_proto.rs`'s module doc).
async fn ws_connect(addr: SocketAddr) -> TcpStream {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let request = "GET /aihub HTTP/1.1\r\n\
         Host: 127.0.0.1\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Version: 13\r\n\r\n";
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        stream.read_exact(&mut byte).await.unwrap();
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    assert!(String::from_utf8_lossy(&buf).contains("101 Switching Protocols"));
    stream
}

async fn ws_send_client(stream: &mut TcpStream, msg: ClientMessage) {
    let text = serde_json::to_string(&IpcMessage::Client(msg)).unwrap();
    ws_proto::write_message(stream, &WsMessage::Text(text))
        .await
        .unwrap();
}

async fn ws_recv_daemon(stream: &mut TcpStream) -> DaemonMessage {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match ws_proto::read_message(stream).await.unwrap() {
                WsMessage::Text(t) => match serde_json::from_str::<IpcMessage>(&t).unwrap() {
                    IpcMessage::Daemon(m) => return m,
                    _ => panic!("wrong direction"),
                },
                WsMessage::Ping(payload) => {
                    ws_proto::write_message(stream, &WsMessage::Pong(payload))
                        .await
                        .unwrap();
                }
                other => panic!("unexpected control frame: {other:?}"),
            }
        }
    })
    .await
    .unwrap()
}

/// Full network handshake (Hello -> Challenge -> signed Hello -> Hello) over the WS control
/// channel, mirroring `aihubd/tests/auth.rs`'s TCP `admit`.
async fn admit(stream: &mut TcpStream, key: &SigningKey) {
    ws_send_client(
        stream,
        ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            credential: None,
        },
    )
    .await;
    let nonce = match ws_recv_daemon(stream).await {
        DaemonMessage::Challenge { nonce } => nonce.into_inner(),
        other => panic!("expected Challenge, got {other:?}"),
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    ws_send_client(
        stream,
        ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            credential: Some(credential_for(key, &nonce, now)),
        },
    )
    .await;
    assert!(matches!(
        ws_recv_daemon(stream).await,
        DaemonMessage::Hello { version } if version == PROTOCOL_VERSION
    ));
}

/// Builds a `Daemon` with one pre-registered session owned by `owner`, backed by a PTY whose
/// output is driven by `output` and whose input hits `try_write` (counted in the returned
/// counter) — the same "inject a fake `Pty` through the `Spawner` seam" pattern
/// `aihubd/tests/socket.rs` uses, never a real harness (ADR §8, spawn gated).
async fn daemon_with_session(
    output: broadcast::Sender<Vec<u8>>,
    scrollback: Vec<u8>,
    owner: aihubd::auth::PrincipalId,
) -> (Daemon, SessionId, Arc<AtomicUsize>) {
    let write_calls = Arc::new(AtomicUsize::new(0));
    let calls_for_spawner = write_calls.clone();
    let daemon = Daemon::new(
        || async { vec![] },
        move |_, _, _| {
            Ok(dummy_pty_with(
                output.clone(),
                scrollback.clone(),
                calls_for_spawner.clone(),
            ))
        },
    );
    let id = SessionId::generate();
    let wt = aihub_git::SessionWorktree {
        session_id: id.clone(),
        path: PathBuf::from("/fake/wt"),
        branch: id.branch_name(),
        base_branch: "main".into(),
        originating_checkout: PathBuf::from("/fake/repo"),
    };
    daemon
        .add_session_owned(
            PathBuf::from("/fake/repo"),
            wt,
            HarnessId::Codex,
            None,
            owner,
        )
        .await
        .unwrap();
    (daemon, id, write_calls)
}

async fn attach_and_get_ticket(
    stream: &mut TcpStream,
    id: &SessionId,
    last_seen_offset: Option<u64>,
) -> DaemonMessage {
    ws_send_client(
        stream,
        ClientMessage::Attach {
            target: SessionTarget::Id(id.clone()),
            last_seen_offset,
        },
    )
    .await;
    // `QuotaPush` (sent right on registration) and, once already attached from a prior call,
    // live `PtyOutput` broadcasts can both land ahead of `Attached` — skip them, same as the
    // local-socket tests already do.
    loop {
        match ws_recv_daemon(stream).await {
            DaemonMessage::QuotaPush { .. } | DaemonMessage::PtyOutput { .. } => continue,
            other => return other,
        }
    }
}

/// Done-when: "canal de controle e canal de PTY separados; teste de que burst de PTY não
/// atrasa RPC de controle". The PTY channel's initial catch-up frame is a ~1.5 MiB ring
/// snapshot; the test client for that channel never reads it, so the daemon's write to that
/// socket blocks on TCP backpressure. Meanwhile the control channel — a fully separate TCP
/// connection and task — must still answer `ListSessions` promptly: the old single-socket
/// design (01-transporte-e-sessao.md §1.2) would have serialized both and hung here.
#[tokio::test]
async fn pty_burst_does_not_delay_control_channel() {
    let key = random_signing_key();
    let owner = aihubd::auth::PrincipalId::from_public_key(&key.verifying_key().to_bytes());
    let (output, _) = broadcast::channel(16);
    let big_backlog = vec![0x42u8; 1_500_000];
    let (daemon, id, _) = daemon_with_session(output, big_backlog, owner.clone()).await;
    let allowlist = aihubd::auth::Allowlist::from_keys([key.verifying_key().to_bytes()]);
    let daemon = daemon.with_allowlist(allowlist);
    let (addr, _stop) = spawn_ws_daemon(daemon).await;

    let mut control = ws_connect(addr).await;
    admit(&mut control, &key).await;
    let ticket = match attach_and_get_ticket(&mut control, &id, None).await {
        DaemonMessage::Attached {
            channel_ticket,
            gap_detected,
            ..
        } => {
            assert!(
                gap_detected,
                "first Attach with no offset is always a full snapshot"
            );
            channel_ticket.expect("network Attach must issue a channel_ticket")
        }
        other => panic!("expected Attached, got {other:?}"),
    };

    // Open the PTY channel and immediately stop reading from it: the ~1.5 MiB catch-up frame
    // the daemon is about to write will eventually fill the OS socket buffer and block.
    let mut pty_channel = ws_connect(addr).await;
    let hello = PtyChannelHello {
        session_id: id.clone(),
        ticket,
    };
    let text = serde_json::to_string(&hello).unwrap();
    ws_proto::write_message(&mut pty_channel, &WsMessage::Text(text))
        .await
        .unwrap();
    // Give the daemon's writer task a moment to start (and likely block on) the big frame.
    tokio::time::sleep(Duration::from_millis(200)).await;

    ws_send_client(&mut control, ClientMessage::ListSessions).await;
    let response = tokio::time::timeout(Duration::from_secs(2), ws_recv_daemon(&mut control))
        .await
        .expect("control RPC must answer promptly despite the blocked PTY channel");
    match response {
        DaemonMessage::SessionList { sessions } => assert_eq!(sessions.len(), 1),
        other => panic!("expected SessionList, got {other:?}"),
    }
    drop(pty_channel);
}

/// Done-when: "`channel_ticket` emitido no controle e exigido no PTY; teste de reuso recusado
/// e de PTY sem ticket recusado".
#[tokio::test]
async fn channel_ticket_is_single_use_and_required() {
    let key = random_signing_key();
    let owner = aihubd::auth::PrincipalId::from_public_key(&key.verifying_key().to_bytes());
    let (output, _) = broadcast::channel(16);
    let (daemon, id, _) = daemon_with_session(output, b"hi".to_vec(), owner.clone()).await;
    let allowlist = aihubd::auth::Allowlist::from_keys([key.verifying_key().to_bytes()]);
    let daemon = daemon.with_allowlist(allowlist);
    let (addr, _stop) = spawn_ws_daemon(daemon).await;

    let mut control = ws_connect(addr).await;
    admit(&mut control, &key).await;
    let ticket = match attach_and_get_ticket(&mut control, &id, None).await {
        DaemonMessage::Attached { channel_ticket, .. } => {
            channel_ticket.expect("must issue a ticket")
        }
        other => panic!("expected Attached, got {other:?}"),
    };

    // First redemption succeeds: the catch-up frame arrives.
    let mut first = ws_connect(addr).await;
    let hello = PtyChannelHello {
        session_id: id.clone(),
        ticket: ticket.clone(),
    };
    ws_proto::write_message(
        &mut first,
        &WsMessage::Text(serde_json::to_string(&hello).unwrap()),
    )
    .await
    .unwrap();
    match ws_proto::read_message(&mut first).await.unwrap() {
        WsMessage::Binary(frame) => {
            let (decoded, _) = PtyBinaryFrame::decode(&frame).unwrap();
            assert_eq!(decoded.data, b"hi".to_vec());
        }
        other => panic!("expected the catch-up binary frame, got {other:?}"),
    }

    // Reusing the same ticket must be refused: no bytes, connection closes.
    let mut reused = ws_connect(addr).await;
    ws_proto::write_message(
        &mut reused,
        &WsMessage::Text(serde_json::to_string(&hello).unwrap()),
    )
    .await
    .unwrap();
    let outcome =
        tokio::time::timeout(Duration::from_secs(2), ws_proto::read_message(&mut reused)).await;
    match outcome {
        Ok(Ok(WsMessage::Close)) | Ok(Err(_)) | Err(_) => {}
        Ok(Ok(other)) => panic!("reused ticket must not see any session bytes, got {other:?}"),
    }

    // No ticket at all (bogus bytes) must also be refused.
    let mut no_ticket = ws_connect(addr).await;
    let bogus = PtyChannelHello {
        session_id: id.clone(),
        ticket: ChannelTicket(Base64Bytes::new(vec![0u8; 32])),
    };
    ws_proto::write_message(
        &mut no_ticket,
        &WsMessage::Text(serde_json::to_string(&bogus).unwrap()),
    )
    .await
    .unwrap();
    let outcome = tokio::time::timeout(
        Duration::from_secs(2),
        ws_proto::read_message(&mut no_ticket),
    )
    .await;
    match outcome {
        Ok(Ok(WsMessage::Close)) | Ok(Err(_)) | Err(_) => {}
        Ok(Ok(other)) => panic!("missing ticket must not see any session bytes, got {other:?}"),
    }
}

/// Done-when: "ring-buffer de 2 MiB com `stream_offset` monotônico; teste de delta após
/// queda curta" + "teste de queda longa (> 2 MiB): `gap_detected: true` e snapshot completo".
#[tokio::test]
async fn attach_delta_after_short_drop_and_gap_after_long_drop() {
    let key = random_signing_key();
    let owner = aihubd::auth::PrincipalId::from_public_key(&key.verifying_key().to_bytes());
    let (output, _) = broadcast::channel(16);
    let (daemon, id, _) = daemon_with_session(output.clone(), vec![], owner.clone()).await;
    let allowlist = aihubd::auth::Allowlist::from_keys([key.verifying_key().to_bytes()]);
    let daemon = daemon.with_allowlist(allowlist);
    let (addr, _stop) = spawn_ws_daemon(daemon).await;

    let mut control = ws_connect(addr).await;
    admit(&mut control, &key).await;

    // First Attach establishes the baseline (no data pushed yet: offset 0).
    match attach_and_get_ticket(&mut control, &id, None).await {
        DaemonMessage::Attached { stream_offset, .. } => assert_eq!(stream_offset, 0),
        other => panic!("expected Attached, got {other:?}"),
    }

    // Short drop: emit a small, well-within-cap amount of output, then reattach with the
    // offset from before the drop.
    output.send(b"hello ".to_vec()).unwrap();
    output.send(b"world".to_vec()).unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    match attach_and_get_ticket(&mut control, &id, Some(0)).await {
        DaemonMessage::Attached {
            scrollback,
            gap_detected,
            stream_offset,
            ..
        } => {
            assert!(!gap_detected, "offset 0 is still within the 2 MiB window");
            assert_eq!(scrollback, b"hello world".to_vec().into());
            assert_eq!(stream_offset, 11);
        }
        other => panic!("expected Attached, got {other:?}"),
    }

    // Long drop: blow well past the 2 MiB cap, then reattach with the now-stale offset 0.
    output.send(vec![0x7Au8; 3 * 1024 * 1024]).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    match attach_and_get_ticket(&mut control, &id, Some(0)).await {
        DaemonMessage::Attached {
            scrollback,
            gap_detected,
            ..
        } => {
            assert!(gap_detected, "offset 0 fell outside the retained window");
            assert_eq!(
                scrollback.len(),
                aihub_core_ring_buffer_cap(),
                "gap snapshot must be exactly the retained window, not more"
            );
        }
        other => panic!("expected Attached, got {other:?}"),
    }
}

/// `RING_BUFFER_CAP` is a private constant of `aihubd`; the test only needs to know its value
/// matches the design's 2 MiB, which it asserts independently here rather than importing it.
fn aihub_core_ring_buffer_cap() -> usize {
    2 * 1024 * 1024
}

/// Done-when: "teste de liveness: timeout de 15s derruba o transporte e o processo filho
/// continua vivo". Never sends anything after the handshake — no client `Ping`, no traffic —
/// so the daemon's own per-read `tokio::time::timeout(15s, ...)` must fire and drop the
/// connection without touching the session (`try_write` calls stay at 0, the session is still
/// attachable afterward).
#[tokio::test]
async fn liveness_timeout_drops_transport_without_touching_session() {
    let key = random_signing_key();
    let owner = aihubd::auth::PrincipalId::from_public_key(&key.verifying_key().to_bytes());
    let (output, _) = broadcast::channel(16);
    let (daemon, id, write_calls) =
        daemon_with_session(output, b"alive".to_vec(), owner.clone()).await;
    let allowlist = aihubd::auth::Allowlist::from_keys([key.verifying_key().to_bytes()]);
    let daemon = daemon.with_allowlist(allowlist);
    let (addr, _stop) = spawn_ws_daemon(daemon.clone()).await;

    let mut control = ws_connect(addr).await;
    admit(&mut control, &key).await;
    attach_and_get_ticket(&mut control, &id, None).await;

    // Silence: no Pong, no message, for longer than the 15s liveness timeout. The daemon's
    // own 5s heartbeat Ping will arrive on the wire but we never answer it, and never send
    // anything ourselves either.
    let dropped = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            match ws_proto::read_message(&mut control).await {
                Ok(WsMessage::Ping(_)) => continue, // deliberately not ponging back
                Ok(_) => continue,
                Err(_) => break, // the daemon closed the transport
            }
        }
    })
    .await;
    assert!(
        dropped.is_ok(),
        "the 15s liveness timeout must close the connection well within 20s"
    );
    assert_eq!(
        write_calls.load(Ordering::SeqCst),
        0,
        "liveness timeout must never write to the child PTY"
    );

    // A fresh connection can still attach the very same session: it was never touched.
    let mut second = ws_connect(addr).await;
    admit(&mut second, &key).await;
    match attach_and_get_ticket(&mut second, &id, None).await {
        DaemonMessage::Attached { scrollback, .. } => {
            assert_eq!(scrollback, b"alive".to_vec().into())
        }
        other => panic!("expected Attached, got {other:?}"),
    }
}

/// Done-when: "`sessions.json` escrito e lido no arranque; teste de reconciliação de PID
/// órfão" — exercised through the real `run_ws` startup path this time (the unit tests in
/// `sessions_catalog.rs` cover the module in isolation).
#[tokio::test]
async fn run_ws_reconciles_orphan_catalog_at_startup() {
    use std::os::unix::process::CommandExt;
    let dir = test_dir();
    let catalog_path = dir.join("sessions.json");

    let mut child = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg("exec sleep 60")
        .process_group(0)
        .spawn()
        .unwrap();
    let pid = child.id() as i32;
    let reaper = std::thread::spawn(move || {
        let _ = child.wait();
    });

    let mut catalog = aihubd::sessions_catalog::SessionsCatalog::default();
    catalog.upsert(aihubd::sessions_catalog::SessionRecord {
        session_id: SessionId::new("orphan-from-prior-run"),
        pid: Some(pid),
        owner: "local".into(),
        repo_path: PathBuf::from("/repo"),
        worktree_path: PathBuf::from("/wt"),
        branch: "session/x".into(),
        harness: HarnessId::ClaudeCode,
    });
    catalog.save(&catalog_path).unwrap();

    let daemon = Daemon::new(|| async { vec![] }, |_, _, _| panic!("no spawn"))
        .with_sessions_catalog_path(catalog_path.clone());
    let (_addr, _stop) = spawn_ws_daemon(daemon).await;

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let alive = tokio::process::Command::new("kill")
                .arg("-0")
                .arg(pid.to_string())
                .output()
                .await
                .map(|o| o.status.success())
                .unwrap_or(false);
            if !alive {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("run_ws startup must reconcile (terminate) the orphaned pid");
    reaper.join().unwrap();

    let after = aihubd::sessions_catalog::SessionsCatalog::load(&catalog_path);
    assert!(
        after.0.is_empty(),
        "catalog must be cleared after startup reconciliation"
    );
    std::fs::remove_dir_all(&dir).ok();
}
