//! Remote WebSocket transport for `aihub --daemon <URL>` (ADR §2.2, Fatia 2).
//!
//! `tokio-tungstenite`'s async `WebSocketStream` only becomes readable/writable
//! through `futures_util::{StreamExt, SinkExt}`, and this workspace does not
//! declare `futures-util` as a direct dependency of `aihub` (session 01's
//! `Cargo.toml` change did not add it, and this session may not edit
//! `Cargo.toml` — see the session result for the `remaining` note). The plain
//! synchronous `tungstenite::WebSocket` has no such requirement: it exposes
//! ordinary blocking `read()`/`send()`. So each WebSocket connection (control
//! or PTY) gets one dedicated OS thread running blocking I/O, bridged into the
//! async TUI event loop through channels — the same "one task per connection,
//! talk to it over a channel" shape as `connection::spawn_daemon_reader`
//! already uses for the local Unix socket.

use aihub_core::{ChannelTicket, ClientMessage, DaemonMessage, IpcMessage, SessionId};
use std::io::ErrorKind;
use std::net::TcpStream;
use std::sync::mpsc as sync_mpsc;
use std::time::Duration;
use tokio::sync::mpsc as tokio_mpsc;
use tokio_tungstenite::tungstenite;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::Message;

/// A synchronous WebSocket over a plain-or-TLS TCP stream, as produced by
/// `tungstenite::connect`.
pub type WsSocket = tungstenite::WebSocket<MaybeTlsStream<TcpStream>>;

/// Names a class of remote route/handshake failure (ADR gap §4.2).
///
/// The daemon's own auth failure is deliberately opaque (`DaemonMessage::Unauthorized`
/// carries no fields), so none of these variants are ever derived from what the daemon
/// says. They come only from what this Mac's own DNS resolver, TCP stack, or TLS layer
/// observed while trying to reach it — the banner tells the owner more than the daemon
/// is willing to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    /// The hostname in `--daemon <URL>` did not resolve.
    DnsResolution,
    /// TCP connection to the resolved address was refused (tunnel down / port closed).
    TunnelRefused,
    /// TLS handshake failed (certificate, protocol, or trust chain issue).
    TlsHandshake,
    /// A response arrived but rejected the WebSocket upgrade (daemon busy / not a WS endpoint).
    HandshakeRejected,
    /// No response at all within the connect budget.
    Timeout,
    /// Connected and completed the WebSocket handshake, but the application-level
    /// proof-of-possession handshake was rejected (`DaemonMessage::Unauthorized`):
    /// not yet paired, or the daemon revoked/expired the credential.
    Unauthorized,
}

impl FailureClass {
    /// Human-readable pt-BR banner text naming the failure class (ADR gap §4.2).
    pub fn banner_text(&self) -> &'static str {
        match self {
            FailureClass::DnsResolution => "Falha de rota: DNS não resolveu o endereço do daemon",
            FailureClass::TunnelRefused => "Falha de rota: túnel recusou a conexão",
            FailureClass::TlsHandshake => "Falha de rota: handshake TLS recusado",
            FailureClass::HandshakeRejected => "Falha de handshake: daemon recusou a conexão",
            FailureClass::Timeout => "Falha de rota: daemon ocupado ou sem resposta",
            FailureClass::Unauthorized => "Não pareado: credencial ainda não aprovada",
        }
    }
}

/// A classified remote transport/handshake error.
#[derive(Debug)]
pub struct RemoteError {
    pub class: FailureClass,
    pub message: String,
}

impl std::fmt::Display for RemoteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.class.banner_text(), self.message)
    }
}

impl std::error::Error for RemoteError {}

impl RemoteError {
    fn new(class: FailureClass, message: impl Into<String>) -> Self {
        Self {
            class,
            message: message.into(),
        }
    }
}

fn classify_io(err: &std::io::Error) -> FailureClass {
    match err.kind() {
        ErrorKind::ConnectionRefused => FailureClass::TunnelRefused,
        ErrorKind::TimedOut => FailureClass::Timeout,
        _ => {
            let msg = err.to_string().to_lowercase();
            if msg.contains("dns")
                || msg.contains("lookup")
                || msg.contains("nodename")
                || msg.contains("resolve")
                || msg.contains("name or service not known")
            {
                FailureClass::DnsResolution
            } else if msg.contains("certificate") || msg.contains("tls") {
                FailureClass::TlsHandshake
            } else {
                FailureClass::TunnelRefused
            }
        }
    }
}

/// Classifies a connect-time transport error into a named failure class.
pub fn classify_connect_error(err: &tungstenite::Error) -> FailureClass {
    match err {
        tungstenite::Error::Io(io_err) => classify_io(io_err),
        tungstenite::Error::Tls(_) => FailureClass::TlsHandshake,
        tungstenite::Error::Http(resp) => match resp.status().as_u16() {
            401 | 403 => FailureClass::Unauthorized,
            _ => FailureClass::HandshakeRejected,
        },
        // `tungstenite::client::connect_to_some` discards the underlying
        // `io::Error` (which would carry `ConnectionRefused`) and reports a
        // bare `UnableToConnect(uri)` instead — this is the actual shape a
        // TCP-level refusal against a resolved address takes on the wire.
        tungstenite::Error::Url(tungstenite::error::UrlError::UnableToConnect(_)) => {
            FailureClass::TunnelRefused
        }
        _ => FailureClass::HandshakeRejected,
    }
}

/// Opens a WebSocket connection to `url`, blocking on a dedicated thread and
/// bounding the wait with `timeout` (ADR gap §4.2: an unresponsive daemon
/// must not hang the reconnect machine forever, it must report `Timeout`).
pub async fn connect(url: &str, timeout: Duration) -> Result<WsSocket, RemoteError> {
    let (socket, _response) = connect_with_response(url, timeout).await?;
    Ok(socket)
}

/// Like [`connect`], but also returns the HTTP upgrade response — used by
/// `aihub doctor --remote` to read the `Date` response header for clock-skew
/// diagnostics (ADR gap §4.1) without adding a new wire message.
pub async fn connect_with_response(
    url: &str,
    timeout: Duration,
) -> Result<(WsSocket, tungstenite::http::Response<Option<Vec<u8>>>), RemoteError> {
    let url = url.to_string();
    let attempt = tokio::task::spawn_blocking(move || tungstenite::connect(url).map_err(Box::new));
    match tokio::time::timeout(timeout, attempt).await {
        Err(_) => Err(RemoteError::new(
            FailureClass::Timeout,
            "tempo esgotado ao conectar ao daemon",
        )),
        Ok(Err(join_err)) => Err(RemoteError::new(
            FailureClass::HandshakeRejected,
            join_err.to_string(),
        )),
        Ok(Ok(Err(ws_err))) => Err(RemoteError::new(
            classify_connect_error(&ws_err),
            ws_err.to_string(),
        )),
        Ok(Ok(Ok(pair))) => Ok(pair),
    }
}

/// Parses an HTTP-date response header (RFC 7231 §7.1.1.1, always the fixed
/// `"Wed, 21 Oct 2015 07:28:00 GMT"` form) into Unix epoch seconds.
pub fn parse_http_date(s: &str) -> Option<u64> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() != 6 {
        return None;
    }
    let day: u64 = parts[1].parse().ok()?;
    let month = match parts[2] {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: u64 = parts[3].parse().ok()?;
    let mut hms = parts[4].split(':');
    let hour: u64 = hms.next()?.parse().ok()?;
    let min: u64 = hms.next()?.parse().ok()?;
    let sec: u64 = hms.next()?.parse().ok()?;

    let days_since_epoch = days_from_civil(year, month, day);
    Some((days_since_epoch as u64) * 86_400 + hour * 3600 + min * 60 + sec)
}

/// Howard Hinnant's `days_from_civil` algorithm: proleptic Gregorian date to
/// days since the Unix epoch (1970-01-01), valid for any year this parser
/// will ever see. No calendar crate is a workspace dependency, so this is
/// the entire "date math" surface this module needs.
fn days_from_civil(y: u64, m: u64, d: u64) -> i64 {
    let y = y as i64 - i64::from(m <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64; // [0, 399]
    let mp = (m + 9) % 12; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe as i64 - 719_468
}

/// Handle to a WebSocket connection driven by its own blocking I/O thread:
/// send outbound frames through `outbound`, receive inbound frames from `inbound`.
pub struct SocketIo {
    pub outbound: sync_mpsc::Sender<Message>,
    pub inbound: tokio_mpsc::UnboundedReceiver<Result<Message, RemoteError>>,
}

/// Spawns the dedicated I/O thread for `socket` and returns channel handles to it.
///
/// The thread interleaves outbound sends with inbound reads using a short read
/// timeout on the plain-TCP path so a queued keystroke never waits behind a
/// blocking read for server data.
///
/// All `MaybeTlsStream` variants get a short read timeout so outbound frames
/// (Hello, keystrokes) are not stuck behind a blocking read — required for
/// `wss://` through Cloudflare (NativeTls on Mac).

fn set_ws_read_timeout(socket: &mut WsSocket, timeout: Duration) {
    match socket.get_mut() {
        MaybeTlsStream::Plain(tcp) => {
            let _ = tcp.set_read_timeout(Some(timeout));
        }
        MaybeTlsStream::NativeTls(tls) => {
            let _ = tls.get_mut().set_read_timeout(Some(timeout));
        }
        // `MaybeTlsStream` is `#[non_exhaustive]`; ignore unknown TLS backends.
        _ => {}
    }
}

pub fn spawn_io(mut socket: WsSocket) -> SocketIo {
    let (outbound_tx, outbound_rx) = sync_mpsc::channel::<Message>();
    let (inbound_tx, inbound_rx) = tokio_mpsc::unbounded_channel();

    std::thread::spawn(move || {
        // Without a short read timeout, the TLS variants block forever in
        // `socket.read()` and never drain `outbound_rx` — so Hello never leaves
        // the Mac and the daemon never answers (deadlock over wss://). Plain TCP
        // already had the 100ms timeout; extend it to NativeTls/Rustls.
        set_ws_read_timeout(&mut socket, Duration::from_millis(100));

        loop {
            loop {
                match outbound_rx.try_recv() {
                    Ok(msg) => {
                        if let Err(e) = socket.send(msg) {
                            let _ = inbound_tx.send(Err(RemoteError::new(
                                classify_connect_error(&e),
                                e.to_string(),
                            )));
                            return;
                        }
                    }
                    Err(sync_mpsc::TryRecvError::Empty) => break,
                    Err(sync_mpsc::TryRecvError::Disconnected) => return,
                }
            }

            match socket.read() {
                Ok(msg) => {
                    if inbound_tx.send(Ok(msg)).is_err() {
                        return;
                    }
                }
                Err(tungstenite::Error::Io(ref io_err))
                    if matches!(io_err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
                {
                    continue;
                }
                Err(e) => {
                    let _ = inbound_tx.send(Err(RemoteError::new(
                        classify_connect_error(&e),
                        e.to_string(),
                    )));
                    return;
                }
            }
        }
    });

    SocketIo {
        outbound: outbound_tx,
        inbound: inbound_rx,
    }
}

/// Serializes a `ClientMessage` for the control channel.
pub fn encode_client_message(msg: &ClientMessage) -> Message {
    let json = serde_json::to_string(&IpcMessage::Client(msg.clone()))
        .expect("ClientMessage always serializes");
    Message::Text(json)
}

/// Decodes a control-channel WebSocket frame into a `DaemonMessage`.
pub fn decode_daemon_message(msg: &Message) -> Result<DaemonMessage, RemoteError> {
    let text = match msg {
        Message::Text(t) => t.as_str(),
        Message::Binary(b) => std::str::from_utf8(b)
            .map_err(|e| RemoteError::new(FailureClass::HandshakeRejected, e.to_string()))?,
        other => {
            return Err(RemoteError::new(
                FailureClass::HandshakeRejected,
                format!("unexpected control frame: {other:?}"),
            ))
        }
    };
    let ipc: IpcMessage = serde_json::from_str(text)
        .map_err(|e| RemoteError::new(FailureClass::HandshakeRejected, e.to_string()))?;
    match ipc {
        IpcMessage::Daemon(m) => Ok(m),
        IpcMessage::Client(_) => Err(RemoteError::new(
            FailureClass::HandshakeRejected,
            "unexpected client message received from daemon".to_string(),
        )),
    }
}

/// Performs the v3 proof-of-possession handshake over an already-connected
/// control channel: sends `Hello`, answers a `Challenge` with a signed
/// credential if the daemon issues one, and returns once accepted.
///
/// Returns `Ok(())` once the daemon confirms with `Hello`. Returns
/// `Err(RemoteError { class: Unauthorized, .. })` if the daemon rejects the
/// credential — the caller treats this as "not paired yet" (ADR gap §4.5),
/// not as a fatal error.
pub async fn perform_remote_handshake(
    io: &mut SocketIo,
    identity: &crate::identity::Identity,
    timeout: Duration,
) -> Result<(), RemoteError> {
    send(
        io,
        &ClientMessage::Hello {
            version: aihub_core::PROTOCOL_VERSION,
            credential: None,
        },
    )?;

    match recv_daemon_message(io, timeout).await? {
        DaemonMessage::Hello { .. } => return Ok(()),
        DaemonMessage::Unauthorized => {
            return Err(RemoteError::new(
                FailureClass::Unauthorized,
                "credencial rejeitada",
            ))
        }
        DaemonMessage::Challenge { nonce } => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let credential = identity.credential(nonce.as_slice(), now);
            send(
                io,
                &ClientMessage::Hello {
                    version: aihub_core::PROTOCOL_VERSION,
                    credential: Some(credential),
                },
            )?;
        }
        other => {
            return Err(RemoteError::new(
                FailureClass::HandshakeRejected,
                format!("resposta inesperada do daemon: {other:?}"),
            ))
        }
    }

    match recv_daemon_message(io, timeout).await? {
        DaemonMessage::Hello { .. } => Ok(()),
        DaemonMessage::Unauthorized => Err(RemoteError::new(
            FailureClass::Unauthorized,
            "credencial rejeitada",
        )),
        other => Err(RemoteError::new(
            FailureClass::HandshakeRejected,
            format!("resposta inesperada do daemon: {other:?}"),
        )),
    }
}

fn send(io: &SocketIo, msg: &ClientMessage) -> Result<(), RemoteError> {
    io.outbound
        .send(encode_client_message(msg))
        .map_err(|_| RemoteError::new(FailureClass::HandshakeRejected, "canal de saída encerrado"))
}

async fn recv_daemon_message(
    io: &mut SocketIo,
    timeout: Duration,
) -> Result<DaemonMessage, RemoteError> {
    match tokio::time::timeout(timeout, io.inbound.recv()).await {
        Err(_) => Err(RemoteError::new(
            FailureClass::Timeout,
            "sem resposta do daemon",
        )),
        Ok(None) => Err(RemoteError::new(
            FailureClass::HandshakeRejected,
            "conexão encerrada",
        )),
        Ok(Some(Err(e))) => Err(e),
        Ok(Some(Ok(frame))) => decode_daemon_message(&frame),
    }
}

/// Adapts a control channel's raw inbound frames into decoded `DaemonMessage`s
/// on a fresh channel, so callers only ever see the same
/// `UnboundedReceiver<Result<DaemonMessage>>` shape the local Unix socket path
/// already uses (`connection::spawn_daemon_reader`) — the main event loop does
/// not need to know which transport produced a message.
pub fn spawn_daemon_reader(
    mut inbound: tokio_mpsc::UnboundedReceiver<Result<Message, RemoteError>>,
) -> tokio_mpsc::UnboundedReceiver<anyhow::Result<DaemonMessage>> {
    let (tx, rx) = tokio_mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(item) = inbound.recv().await {
            let mapped = match item {
                Ok(frame) => {
                    decode_daemon_message(&frame).map_err(|e| anyhow::anyhow!(e.to_string()))
                }
                Err(e) => Err(anyhow::anyhow!(e.to_string())),
            };
            let is_err = mapped.is_err();
            if tx.send(mapped).is_err() || is_err {
                return;
            }
        }
    });
    rx
}

/// Opens the secondary PTY data channel (ADR contradiction 1, Option B), presenting
/// the `channel_ticket` minted by the authenticated control connection.
pub async fn connect_pty_channel(
    pty_url: &str,
    session_id: &SessionId,
    ticket: ChannelTicket,
    timeout: Duration,
) -> Result<SocketIo, RemoteError> {
    let socket = connect(pty_url, timeout).await?;
    let io = spawn_io(socket);

    let hello = aihub_core::PtyChannelHello {
        session_id: session_id.clone(),
        ticket,
    };
    let json = serde_json::to_string(&hello)
        .map_err(|e| RemoteError::new(FailureClass::HandshakeRejected, e.to_string()))?;
    io.outbound
        .send(Message::Text(json))
        .map_err(|_| RemoteError::new(FailureClass::HandshakeRejected, "canal de PTY encerrado"))?;
    Ok(io)
}

/// Derives the dedicated PTY channel URL from the control channel URL by
/// appending a `/pty` path segment.
///
/// This is this session's own convention (ADR contradiction 1 fixes the
/// *ticket* mechanism, not the URL layout of the two connections) — pending
/// alignment with the daemon's network listener once sessions 03/04 land it.
pub fn pty_channel_url(control_url: &str) -> String {
    if control_url.ends_with('/') {
        format!("{control_url}pty")
    } else {
        format!("{control_url}/pty")
    }
}

/// Target daemon this client connects to: the existing local Unix socket
/// path, or a remote WebSocket URL (ADR §2.1, §7).
#[derive(Debug, Clone, PartialEq)]
pub enum RemoteTarget {
    Local(std::path::PathBuf),
    Remote(String),
}

/// Parses the `--daemon <TARGET>` / `AIHUB_DAEMON` value (design doc §2.7):
/// `unix:<path>` or a bare filesystem path is local; `ws://`/`wss://` is remote.
/// `http://`/`https://` are accepted as a convenience and rewritten to `ws://`/`wss://`.
pub fn parse_daemon_target(raw: &str) -> RemoteTarget {
    if let Some(path) = raw.strip_prefix("unix:") {
        return RemoteTarget::Local(std::path::PathBuf::from(path));
    }
    if let Some(rest) = raw.strip_prefix("https://") {
        return RemoteTarget::Remote(format!("wss://{rest}"));
    }
    if let Some(rest) = raw.strip_prefix("http://") {
        return RemoteTarget::Remote(format!("ws://{rest}"));
    }
    if raw.starts_with("ws://") || raw.starts_with("wss://") {
        return RemoteTarget::Remote(raw.to_string());
    }
    RemoteTarget::Local(std::path::PathBuf::from(raw))
}

/// Exponential backoff with jitter for the async reconnection machine
/// (design doc §2.5): 500 ms initial, ×1.5 factor, ±20% jitter, 15 s cap,
/// unbounded attempts.
pub struct Backoff {
    attempt: u32,
}

impl Backoff {
    pub fn new() -> Self {
        Self { attempt: 0 }
    }

    /// Current attempt number (1-indexed), incremented by each `next_delay` call.
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    /// Returns the delay to wait before the next attempt, advancing the counter.
    pub fn next_delay(&mut self) -> Duration {
        self.attempt += 1;
        let base_ms = 500f64 * 1.5f64.powi((self.attempt - 1) as i32);
        let capped_ms = base_ms.min(15_000.0);
        let jitter = 1.0 + (pseudo_jitter(self.attempt) * 0.4 - 0.2); // ±20%
        Duration::from_millis((capped_ms * jitter).max(0.0) as u64)
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

/// Deterministic pseudo-random value in `[0, 1)`, seeded by the attempt count.
/// A real RNG would work too, but backoff jitter has no security requirement
/// and a deterministic function keeps `next_delay` trivially testable.
fn pseudo_jitter(seed: u32) -> f64 {
    let x = (seed.wrapping_mul(2654435761)) as f64;
    (x / u32::MAX as f64).fract().abs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    #[test]
    fn classifies_connection_refused_as_tunnel_refused() {
        let err =
            tungstenite::Error::Io(io::Error::new(io::ErrorKind::ConnectionRefused, "refused"));
        assert_eq!(classify_connect_error(&err), FailureClass::TunnelRefused);
    }

    #[test]
    fn classifies_timed_out_io_as_timeout() {
        let err = tungstenite::Error::Io(io::Error::new(io::ErrorKind::TimedOut, "timed out"));
        assert_eq!(classify_connect_error(&err), FailureClass::Timeout);
    }

    #[test]
    fn classifies_dns_lookup_failure_message_as_dns_resolution() {
        let err = tungstenite::Error::Io(io::Error::other(
            "failed to lookup address information: nodename nor servname provided",
        ));
        assert_eq!(classify_connect_error(&err), FailureClass::DnsResolution);
    }

    #[test]
    fn classifies_certificate_error_message_as_tls_handshake() {
        let err =
            tungstenite::Error::Io(io::Error::other("invalid peer certificate: UnknownIssuer"));
        assert_eq!(classify_connect_error(&err), FailureClass::TlsHandshake);
    }

    #[test]
    fn classifies_unable_to_connect_url_error_as_tunnel_refused() {
        let err = tungstenite::Error::Url(tungstenite::error::UrlError::UnableToConnect(
            "127.0.0.1:1".to_string(),
        ));
        assert_eq!(classify_connect_error(&err), FailureClass::TunnelRefused);
    }

    #[test]
    fn classifies_tls_variant_as_tls_handshake() {
        let err = tungstenite::Error::Tls(tungstenite::error::TlsError::InvalidDnsName);
        assert_eq!(classify_connect_error(&err), FailureClass::TlsHandshake);
    }

    #[test]
    fn classifies_http_401_as_unauthorized() {
        let resp = tungstenite::http::Response::builder()
            .status(401)
            .body(None)
            .unwrap();
        let err = tungstenite::Error::Http(resp);
        assert_eq!(classify_connect_error(&err), FailureClass::Unauthorized);
    }

    #[test]
    fn classifies_other_http_status_as_handshake_rejected() {
        let resp = tungstenite::http::Response::builder()
            .status(503)
            .body(None)
            .unwrap();
        let err = tungstenite::Error::Http(resp);
        assert_eq!(
            classify_connect_error(&err),
            FailureClass::HandshakeRejected
        );
    }

    #[test]
    fn every_failure_class_has_distinct_banner_text() {
        let classes = [
            FailureClass::DnsResolution,
            FailureClass::TunnelRefused,
            FailureClass::TlsHandshake,
            FailureClass::HandshakeRejected,
            FailureClass::Timeout,
            FailureClass::Unauthorized,
        ];
        let mut texts: Vec<&str> = classes.iter().map(|c| c.banner_text()).collect();
        let before = texts.len();
        texts.sort_unstable();
        texts.dedup();
        assert_eq!(
            texts.len(),
            before,
            "failure classes must have distinct banner text"
        );
    }

    #[test]
    fn backoff_starts_at_500ms_and_caps_at_15s() {
        let mut backoff = Backoff::new();
        let first = backoff.next_delay();
        assert!(first.as_millis() >= 400 && first.as_millis() <= 600);

        let mut last = first;
        for _ in 0..20 {
            last = backoff.next_delay();
        }
        assert!(last.as_millis() <= 18_000); // capped 15s + jitter headroom
    }

    #[test]
    fn backoff_reset_returns_to_first_step() {
        let mut backoff = Backoff::new();
        for _ in 0..5 {
            backoff.next_delay();
        }
        backoff.reset();
        assert_eq!(backoff.attempt(), 0);
        let after_reset = backoff.next_delay();
        assert!(after_reset.as_millis() >= 400 && after_reset.as_millis() <= 600);
    }

    #[test]
    fn parses_daemon_target_schemes() {
        assert_eq!(
            parse_daemon_target("unix:/tmp/aihub.sock"),
            RemoteTarget::Local(std::path::PathBuf::from("/tmp/aihub.sock"))
        );
        assert_eq!(
            parse_daemon_target("/tmp/aihub.sock"),
            RemoteTarget::Local(std::path::PathBuf::from("/tmp/aihub.sock"))
        );
        assert_eq!(
            parse_daemon_target("wss://aihub.mathai.com.br"),
            RemoteTarget::Remote("wss://aihub.mathai.com.br".to_string())
        );
        assert_eq!(
            parse_daemon_target("https://aihub.mathai.com.br"),
            RemoteTarget::Remote("wss://aihub.mathai.com.br".to_string())
        );
    }

    #[test]
    fn parses_reference_http_date() {
        // RFC 7231 example date, whose epoch value is well known.
        assert_eq!(
            parse_http_date("Wed, 21 Oct 2015 07:28:00 GMT"),
            Some(1_445_412_480)
        );
    }

    #[test]
    fn parses_unix_epoch_date() {
        assert_eq!(parse_http_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
    }

    #[test]
    fn rejects_malformed_http_date() {
        assert_eq!(parse_http_date("not a date"), None);
    }

    #[test]
    fn pty_url_appends_path_segment() {
        assert_eq!(pty_channel_url("wss://host:9920"), "wss://host:9920/pty");
        assert_eq!(pty_channel_url("wss://host:9920/"), "wss://host:9920/pty");
    }
}
