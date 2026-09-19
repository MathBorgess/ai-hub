//! Integration tests for the remote WebSocket transport (`aihub --daemon <URL>`):
//! the v3 proof-of-possession handshake, the PTY dual channel, `doctor --remote`,
//! and the non-blocking property of the async reconnect machine.
//!
//! There is no real `aihubd` network listener yet (sessions 03/04, out of this
//! session's scope), so the "server" side here is a minimal stub speaking the
//! same synchronous `tungstenite` protocol the client uses — see
//! `aihub::remote` for why both sides use blocking `tungstenite::WebSocket`
//! instead of the async `WebSocketStream` (`futures-util` is not a direct
//! workspace dependency).

use aihub::identity::Identity;
use aihub::remote::{self, FailureClass};
use aihub_core::{
    ChannelTicket, ClientCredential, ClientMessage, DaemonMessage, IpcMessage, PtyBinaryFrame,
    PtyChannelHello, SessionId, PROTOCOL_VERSION,
};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;
use tokio_tungstenite::tungstenite;
use tungstenite::{Message, WebSocket};

static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn scratch_identity_path(name: &str) -> std::path::PathBuf {
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    std::env::temp_dir().join(format!(
        "aihub-remote-test-{name}-{}-{n}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn bind_loopback() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    (listener, format!("ws://127.0.0.1:{port}"))
}

fn send_daemon(ws: &mut WebSocket<TcpStream>, msg: DaemonMessage) {
    let json = serde_json::to_string(&IpcMessage::Daemon(msg)).unwrap();
    ws.send(Message::Text(json)).unwrap();
}

fn recv_client(ws: &mut WebSocket<TcpStream>) -> ClientMessage {
    loop {
        match ws.read().unwrap() {
            Message::Text(t) => match serde_json::from_str::<IpcMessage>(&t).unwrap() {
                IpcMessage::Client(c) => return c,
                IpcMessage::Daemon(_) => panic!("unexpected daemon message from test client"),
            },
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("unexpected frame from client: {other:?}"),
        }
    }
}

/// Runs the v3 handshake as the daemon would: challenge, verify the signed
/// credential is well-formed, accept.
fn serve_handshake_accepting(listener: &TcpListener) {
    let (stream, _) = listener.accept().unwrap();
    let mut ws = tungstenite::accept(stream).unwrap();

    match recv_client(&mut ws) {
        ClientMessage::Hello {
            version,
            credential,
        } => {
            assert_eq!(version, PROTOCOL_VERSION);
            assert!(
                credential.is_none(),
                "first Hello must not preempt the challenge"
            );
        }
        other => panic!("expected Hello, got {other:?}"),
    }

    let nonce = b"test-nonce-123".to_vec();
    send_daemon(
        &mut ws,
        DaemonMessage::Challenge {
            nonce: nonce.clone().into(),
        },
    );

    match recv_client(&mut ws) {
        ClientMessage::Hello {
            credential: Some(cred),
            ..
        } => {
            assert_credential_proves_possession(&cred, &nonce);
        }
        other => panic!("expected credentialed Hello, got {other:?}"),
    }

    send_daemon(
        &mut ws,
        DaemonMessage::Hello {
            version: PROTOCOL_VERSION,
        },
    );
}

fn assert_credential_proves_possession(cred: &ClientCredential, nonce: &[u8]) {
    use ed25519_dalek::{Signature, VerifyingKey};
    assert_eq!(cred.audience, aihub_core::CREDENTIAL_AUDIENCE);
    let pk: [u8; 32] = cred.public_key.as_slice().try_into().unwrap();
    let sig: [u8; 64] = cred.signature.as_slice().try_into().unwrap();
    let vk = VerifyingKey::from_bytes(&pk).unwrap();
    let sig = Signature::from_bytes(&sig);
    assert!(
        vk.verify_strict(nonce, &sig).is_ok(),
        "credential must sign the daemon's nonce"
    );
}

/// Runs the v3 handshake as the daemon would when the credential is not (yet)
/// approved: challenge, then `Unauthorized` regardless of the signature.
fn serve_handshake_rejecting(listener: &TcpListener) {
    let (stream, _) = listener.accept().unwrap();
    let mut ws = tungstenite::accept(stream).unwrap();

    recv_client(&mut ws); // Hello
    send_daemon(
        &mut ws,
        DaemonMessage::Challenge {
            nonce: b"n".to_vec().into(),
        },
    );
    recv_client(&mut ws); // credentialed Hello
    send_daemon(&mut ws, DaemonMessage::Unauthorized);
}

#[tokio::test]
async fn remote_handshake_succeeds_with_signed_credential() {
    let (listener, url) = bind_loopback();
    let server = std::thread::spawn(move || serve_handshake_accepting(&listener));

    let identity_path = scratch_identity_path("accept");
    let identity = Identity::load_or_create(&identity_path).unwrap();

    let socket = remote::connect(&url, Duration::from_secs(5)).await.unwrap();
    let mut io = remote::spawn_io(socket);
    let result = remote::perform_remote_handshake(&mut io, &identity, Duration::from_secs(5)).await;

    assert!(result.is_ok(), "expected handshake to succeed: {result:?}");
    server.join().unwrap();
    let _ = std::fs::remove_file(&identity_path);
}

/// Regression: after Hello the daemon's control loop sends a WebSocket Ping on the
/// first tokio interval tick. That Ping must not tear down spawn_daemon_reader
/// (previously decode_daemon_message treated it as a fatal unexpected frame).
#[tokio::test]
async fn control_channel_survives_post_handshake_ping() {
    let (listener, url) = bind_loopback();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut ws = tungstenite::accept(stream).unwrap();
        // Minimal accepting handshake (same shape as serve_handshake_accepting).
        match recv_client(&mut ws) {
            ClientMessage::Hello { credential: None, .. } => {}
            other => panic!("expected bare Hello, got {other:?}"),
        }
        send_daemon(
            &mut ws,
            DaemonMessage::Challenge {
                nonce: b"ping-regression-nonce".to_vec().into(),
            },
        );
        match recv_client(&mut ws) {
            ClientMessage::Hello {
                credential: Some(_),
                ..
            } => {}
            other => panic!("expected credentialed Hello, got {other:?}"),
        }
        send_daemon(
            &mut ws,
            DaemonMessage::Hello {
                version: PROTOCOL_VERSION,
            },
        );
        // Emulate aihubd control_channel heartbeat: Ping immediately after Hello.
        ws.send(Message::Ping(b"hb".to_vec())).unwrap();
        // Then a real daemon payload the reader must still deliver.
        send_daemon(
            &mut ws,
            DaemonMessage::Hello {
                version: PROTOCOL_VERSION,
            },
        );
        // Expect a Pong (or at least no client disconnect); drain briefly.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut saw_pong = false;
        while std::time::Instant::now() < deadline {
            let _ = ws.get_mut().set_read_timeout(Some(Duration::from_millis(100)));
            match ws.read() {
                Ok(Message::Pong(_)) => {
                    saw_pong = true;
                    break;
                }
                Ok(Message::Ping(_)) => continue,
                Ok(_) => continue,
                Err(_) => continue,
            }
        }
        assert!(saw_pong, "client must answer daemon Ping with Pong");
    });

    let identity_path = scratch_identity_path("ping-survive");
    let identity = Identity::load_or_create(&identity_path).unwrap();
    let socket = remote::connect(&url, Duration::from_secs(5)).await.unwrap();
    let mut io = remote::spawn_io(socket);
    remote::perform_remote_handshake(&mut io, &identity, Duration::from_secs(5))
        .await
        .expect("handshake");

    let mut daemon_rx = remote::spawn_daemon_reader(io.inbound);
    let msg = tokio::time::timeout(Duration::from_secs(3), daemon_rx.recv())
        .await
        .expect("reader timed out — Ping likely killed the control channel")
        .expect("reader closed")
        .expect("decode error");
    match msg {
        DaemonMessage::Hello { version } => assert_eq!(version, PROTOCOL_VERSION),
        other => panic!("expected Hello after Ping, got {other:?}"),
    }

    server.join().unwrap();
    let _ = std::fs::remove_file(&identity_path);
}



/// Regression: aihubd tears down the transport after 15s with no inbound frames.
/// Client must emit its own WebSocket Ping so Cloudflare edge ACK of origin Pings
/// cannot starve the daemon reader.
#[tokio::test]
async fn control_channel_client_keepalive_pings() {
    let (listener, url) = bind_loopback();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut ws = tungstenite::accept(stream).unwrap();
        match recv_client(&mut ws) {
            ClientMessage::Hello { credential: None, .. } => {}
            other => panic!("expected bare Hello, got {other:?}"),
        }
        send_daemon(
            &mut ws,
            DaemonMessage::Challenge {
                nonce: b"keepalive-nonce".to_vec().into(),
            },
        );
        match recv_client(&mut ws) {
            ClientMessage::Hello {
                credential: Some(_),
                ..
            } => {}
            other => panic!("expected credentialed Hello, got {other:?}"),
        }
        send_daemon(
            &mut ws,
            DaemonMessage::Hello {
                version: PROTOCOL_VERSION,
            },
        );
        // Do NOT send server Ping — only wait for client keepalive Ping.
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        let mut saw_client_ping = false;
        while std::time::Instant::now() < deadline {
            let _ = ws.get_mut().set_read_timeout(Some(Duration::from_millis(200)));
            match ws.read() {
                Ok(Message::Ping(_)) => {
                    saw_client_ping = true;
                    let _ = ws.send(Message::Pong(vec![]));
                    break;
                }
                Ok(Message::Pong(_)) => continue,
                Ok(_) => continue,
                Err(_) => continue,
            }
        }
        assert!(
            saw_client_ping,
            "idle client must emit WebSocket Ping keepalive within a few seconds"
        );
    });

    let identity_path = scratch_identity_path("keepalive");
    let identity = Identity::load_or_create(&identity_path).unwrap();
    let socket = remote::connect(&url, Duration::from_secs(5)).await.unwrap();
    let mut io = remote::spawn_io(socket);
    remote::perform_remote_handshake(&mut io, &identity, Duration::from_secs(5))
        .await
        .expect("handshake");
    // Hold the SocketIo open (outbound sender alive) while keepalive fires.
    tokio::time::sleep(Duration::from_secs(6)).await;
    drop(io);
    server.join().unwrap();
    let _ = std::fs::remove_file(&identity_path);
}


#[tokio::test]
async fn remote_handshake_reports_unauthorized_when_not_yet_paired() {
    let (listener, url) = bind_loopback();
    let server = std::thread::spawn(move || serve_handshake_rejecting(&listener));

    let identity_path = scratch_identity_path("reject");
    let identity = Identity::load_or_create(&identity_path).unwrap();

    let socket = remote::connect(&url, Duration::from_secs(5)).await.unwrap();
    let mut io = remote::spawn_io(socket);
    let result = remote::perform_remote_handshake(&mut io, &identity, Duration::from_secs(5)).await;

    let err = result.expect_err("unapproved credential must fail");
    assert_eq!(err.class, FailureClass::Unauthorized);
    server.join().unwrap();
    let _ = std::fs::remove_file(&identity_path);
}

#[tokio::test]
async fn connect_reports_tunnel_refused_when_nothing_is_listening() {
    // Bind then immediately drop, freeing the port with nothing listening.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let url = format!("ws://127.0.0.1:{port}");
    let err = remote::connect(&url, Duration::from_secs(5))
        .await
        .expect_err("nothing is listening on this port");
    assert_eq!(err.class, FailureClass::TunnelRefused);
}

#[tokio::test]
async fn connect_times_out_against_a_non_responding_peer() {
    // A listener that accepts the TCP connection but never completes the WS
    // handshake: the client's connect budget must still expire.
    let (listener, url) = bind_loopback();
    let hold = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        std::thread::sleep(Duration::from_secs(2));
        drop(stream);
    });

    let err = remote::connect(&url, Duration::from_millis(200))
        .await
        .expect_err("daemon never completes the handshake within budget");
    assert_eq!(err.class, FailureClass::Timeout);
    let _ = hold.join();
}

#[tokio::test]
async fn pty_channel_streams_binary_frames_after_ticket_handshake() {
    let (listener, url) = bind_loopback();
    let ticket = ChannelTicket(vec![9, 9, 9].into());
    let ticket_for_server = ticket.clone();
    let session_id = SessionId::new("sess_test_pty");
    let session_for_server = session_id.clone();

    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut ws = tungstenite::accept(stream).unwrap();

        let hello: PtyChannelHello = match ws.read().unwrap() {
            Message::Text(t) => serde_json::from_str(&t).unwrap(),
            other => panic!("expected PtyChannelHello, got {other:?}"),
        };
        assert_eq!(hello.session_id, session_for_server);
        assert_eq!(hello.ticket, ticket_for_server);

        let frame = PtyBinaryFrame::new(0, b"hello from pty".to_vec()).encode();
        ws.send(Message::Binary(frame)).unwrap();
    });

    let pty_url = remote::pty_channel_url(&url);
    let mut io = remote::connect_pty_channel(&pty_url, &session_id, ticket, Duration::from_secs(5))
        .await
        .unwrap();

    let received = tokio::time::timeout(Duration::from_secs(5), io.inbound.recv())
        .await
        .expect("did not time out")
        .expect("channel not closed")
        .expect("no transport error");

    match received {
        Message::Binary(bytes) => {
            let (frame, _) = PtyBinaryFrame::decode(&bytes).unwrap();
            assert_eq!(frame.data, b"hello from pty");
        }
        other => panic!("expected binary PTY frame, got {other:?}"),
    }

    server.join().unwrap();
}

#[tokio::test]
async fn doctor_remote_reports_route_failure_class_when_unreachable() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let identity_path = scratch_identity_path("doctor-unreachable");
    let report =
        aihub::doctor::run_remote_doctor(&format!("ws://127.0.0.1:{port}"), &identity_path)
            .await
            .unwrap();

    assert_eq!(
        report.route,
        aihub::doctor::RouteStatus::Failed(FailureClass::TunnelRefused)
    );
    let _ = std::fs::remove_file(&identity_path);
}

#[tokio::test]
async fn doctor_remote_reports_pending_approval_when_daemon_rejects_credential() {
    let (listener, url) = bind_loopback();
    let server = std::thread::spawn(move || serve_handshake_rejecting(&listener));

    let identity_path = scratch_identity_path("doctor-pending");
    let report = aihub::doctor::run_remote_doctor(&url, &identity_path)
        .await
        .unwrap();

    assert_eq!(report.route, aihub::doctor::RouteStatus::Ok);
    match report.pairing {
        aihub::doctor::PairingStatus::PendingApproval { .. } => {}
        other => panic!("expected PendingApproval, got {other:?}"),
    }
    server.join().unwrap();
    let _ = std::fs::remove_file(&identity_path);
}

#[tokio::test]
async fn doctor_remote_reports_approved_pairing_and_protocol_version() {
    let (listener, url) = bind_loopback();
    let server = std::thread::spawn(move || serve_handshake_accepting(&listener));

    let identity_path = scratch_identity_path("doctor-approved");
    let report = aihub::doctor::run_remote_doctor(&url, &identity_path)
        .await
        .unwrap();

    assert_eq!(report.route, aihub::doctor::RouteStatus::Ok);
    assert_eq!(report.protocol_version, Some(PROTOCOL_VERSION));
    match report.pairing {
        aihub::doctor::PairingStatus::Approved { .. } => {}
        other => panic!("expected Approved, got {other:?}"),
    }
    server.join().unwrap();
    let _ = std::fs::remove_file(&identity_path);
}

/// Proves the reconnect backoff never blocks the async runtime: a connect
/// attempt against a black-holed address (backoff sleeping in its own task)
/// must not stop an unrelated concurrent task from making progress — the
/// same non-blocking property the TUI draw loop depends on (design doc §2.5).
#[tokio::test]
async fn backoff_sleep_does_not_block_other_async_work() {
    let ticks = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let ticks_clone = ticks.clone();

    let blocked_attempt = tokio::spawn(async move {
        // Nothing listens on this port; connect fails fast, then the caller
        // (not exercised here) would sleep on `Backoff` between attempts.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let _ = remote::connect(&format!("ws://127.0.0.1:{port}"), Duration::from_secs(1)).await;
        let mut backoff = remote::Backoff::new();
        tokio::time::sleep(backoff.next_delay()).await;
    });

    let ticker = tokio::spawn(async move {
        for _ in 0..5 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            ticks_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    });

    let _ = tokio::join!(blocked_attempt, ticker);
    assert_eq!(ticks.load(std::sync::atomic::Ordering::SeqCst), 5);
}
