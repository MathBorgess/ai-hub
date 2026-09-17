//! Fatia 1 (ADR §2.3, §6): handshake v3, admissão assimétrica por tipo de listener,
//! amarração de sessão ao principal e auditoria durável. Cobre exatamente os itens de
//! `## Done when` do brief da sessão 03 que envolvem I/O de rede real.
use aihub_core::*;
use aihubd::auth::{Allowlist, PrincipalId};
use aihubd::{Daemon, Pty};
use ed25519_dalek::{Signature, Signer, SigningKey};
use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UnixStream},
    sync::broadcast,
};

static SOCKET_COUNTER: AtomicU64 = AtomicU64::new(1);

/// One fresh directory per call, so `bind()`'s own `chmod 0o700` targets a directory this
/// process just created (never the shared system temp root — some sandboxes deny chmod there).
fn test_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ah-auth-{}-{}-{}",
        std::process::id(),
        SOCKET_COUNTER.fetch_add(1, Ordering::SeqCst),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn test_socket() -> PathBuf {
    test_dir().join("s")
}

fn dummy_pty() -> Pty {
    let (tx, rx) = broadcast::channel(16);
    Pty {
        output: rx,
        scrollback: vec![],
        write: Box::new(|_| Box::pin(async { Ok(()) })),
        resize: Box::new(|_| Ok(())),
        wait: Box::new(move || {
            let _keep = tx.clone();
            Box::pin(std::future::pending())
        }),
        kill: Box::new(|| Box::pin(async { Ok(()) })),
        stop: Arc::new(|_| Box::pin(async { Ok(Some(0)) })),
        try_write: Arc::new(|_| Ok(())),
    }
}

fn random_signing_key() -> SigningKey {
    use rand::Rng;
    let mut seed = [0u8; 32];
    rand::rng().fill_bytes(&mut seed);
    SigningKey::from_bytes(&seed)
}

fn credential_for(
    key: &SigningKey,
    nonce: &[u8],
    audience: &str,
    timestamp: u64,
) -> ClientCredential {
    let signature: Signature = key.sign(nonce);
    ClientCredential {
        public_key: Base64Bytes::new(key.verifying_key().to_bytes().to_vec()),
        signature: Base64Bytes::new(signature.to_bytes().to_vec()),
        audience: audience.into(),
        timestamp,
    }
}

// -- Unix socket (local) framing -------------------------------------------------------------

async fn send_uds(s: &mut UnixStream, msg: ClientMessage) {
    s.write_all(&encode_frame(&msg.into()).unwrap())
        .await
        .unwrap();
}
async fn recv_uds(s: &mut UnixStream) -> DaemonMessage {
    tokio::time::timeout(Duration::from_secs(5), async {
        let n = s.read_u32().await.unwrap();
        let mut data = vec![0; n as usize];
        s.read_exact(&mut data).await.unwrap();
        match serde_json::from_slice::<IpcMessage>(&data).unwrap() {
            IpcMessage::Daemon(m) => m,
            _ => panic!("wrong direction"),
        }
    })
    .await
    .unwrap()
}

// -- TCP (network) framing ---------------------------------------------------------------

async fn send_tcp(s: &mut TcpStream, msg: ClientMessage) {
    s.write_all(&encode_frame(&msg.into()).unwrap())
        .await
        .unwrap();
}
async fn recv_tcp(s: &mut TcpStream) -> DaemonMessage {
    tokio::time::timeout(Duration::from_secs(5), async {
        let n = s.read_u32().await.unwrap();
        let mut data = vec![0; n as usize];
        s.read_exact(&mut data).await.unwrap();
        match serde_json::from_slice::<IpcMessage>(&data).unwrap() {
            IpcMessage::Daemon(m) => m,
            _ => panic!("wrong direction"),
        }
    })
    .await
    .unwrap()
}

fn loopback_addr() -> SocketAddr {
    "127.0.0.1:0".parse().unwrap()
}

/// Binds a network listener on an ephemeral loopback port and runs the daemon on it in the
/// background, returning the bound address. Mirrors `run()`'s Unix-socket test pattern, but
/// `run_tcp` needs a pre-bound listener to learn the ephemeral port before spawning.
async fn spawn_network_daemon(daemon: Daemon) -> (SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let listener = TcpListener::bind(loopback_addr()).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        // run_tcp binds its own listener; drop this probe listener first so the port is free
        // for the split second between the two binds (loopback ephemeral ports are reused
        // immediately once released, this is not a race with an external process).
        drop(listener);
        daemon
            .run_tcp(addr, async {
                stop_rx.await.ok();
            })
            .await
            .unwrap();
    });
    // Give the daemon a moment to rebind the same ephemeral port.
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if TcpStream::connect(addr).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("daemon must bind tcp listener within 3s");
    (addr, stop_tx)
}

#[tokio::test]
async fn local_uds_v2_client_without_credential_connects_and_operates() {
    let path = test_socket();
    let daemon = Daemon::new(|| async { vec![] }, |_, _, _| panic!("no spawn"));
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let p = path.clone();
    let d = daemon.clone();
    let task = tokio::spawn(async move {
        d.run(p, async {
            stop_rx.await.ok();
        })
        .await
    });

    let mut s = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(stream) = UnixStream::connect(&path).await {
                break stream;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("connect timed out; path={}", path.display()));

    // A v2 client (no `credential` field at all) is retrocompat: this is the exact shape a
    // pre-Fatia-1 client sends.
    send_uds(
        &mut s,
        ClientMessage::Hello {
            version: 2,
            credential: None,
        },
    )
    .await;
    assert!(matches!(
        recv_uds(&mut s).await,
        DaemonMessage::Hello {
            version: PROTOCOL_VERSION
        }
    ));
    // QuotaPush follows unconditionally; drain it before operating.
    assert!(matches!(
        recv_uds(&mut s).await,
        DaemonMessage::QuotaPush { .. }
    ));
    send_uds(&mut s, ClientMessage::ListSessions).await;
    assert_eq!(
        recv_uds(&mut s).await,
        DaemonMessage::SessionList { sessions: vec![] }
    );

    let _ = stop_tx.send(());
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn tcp_loopback_without_proof_of_possession_gets_unauthorized_and_nothing_else() {
    let daemon = Daemon::new(|| async { vec![] }, |_, _, _| panic!("no spawn"))
        .with_allowlist(Allowlist::empty());
    let (addr, stop_tx) = spawn_network_daemon(daemon).await;

    let mut s = TcpStream::connect(addr).await.unwrap();
    send_tcp(
        &mut s,
        ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            credential: None,
        },
    )
    .await;
    // Network listener always challenges before deciding: this alone must never admit
    // anything (ADR §2.3 §5 — connection-level freshness, not per-frame).
    assert!(matches!(
        recv_tcp(&mut s).await,
        DaemonMessage::Challenge { .. }
    ));

    // No real proof: a self-signed credential naming an unknown key. Still opaque `unauthorized`.
    let key = random_signing_key();
    let bogus = credential_for(&key, b"not-the-real-nonce", CREDENTIAL_AUDIENCE, 0);
    send_tcp(
        &mut s,
        ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            credential: Some(bogus),
        },
    )
    .await;
    assert_eq!(recv_tcp(&mut s).await, DaemonMessage::Unauthorized);

    // "E nada mais": the daemon closes the connection after rejecting it.
    let mut trailing = [0u8; 1];
    let n = tokio::time::timeout(Duration::from_secs(2), s.read(&mut trailing))
        .await
        .expect("connection must be closed promptly after Unauthorized")
        .unwrap();
    assert_eq!(n, 0, "no further bytes after Unauthorized");

    let _ = stop_tx.send(());
}

#[tokio::test]
async fn tcp_loopback_with_valid_allowlisted_proof_of_possession_is_admitted() {
    let key = random_signing_key();
    let allow = Allowlist::from_keys([key.verifying_key().to_bytes()]);
    let daemon =
        Daemon::new(|| async { vec![] }, |_, _, _| panic!("no spawn")).with_allowlist(allow);
    let (addr, stop_tx) = spawn_network_daemon(daemon).await;

    let mut s = TcpStream::connect(addr).await.unwrap();
    send_tcp(
        &mut s,
        ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            credential: None,
        },
    )
    .await;
    let nonce = match recv_tcp(&mut s).await {
        DaemonMessage::Challenge { nonce } => nonce.into_inner(),
        other => panic!("expected Challenge, got {other:?}"),
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let credential = credential_for(&key, &nonce, CREDENTIAL_AUDIENCE, now);
    send_tcp(
        &mut s,
        ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            credential: Some(credential),
        },
    )
    .await;
    assert!(matches!(
        recv_tcp(&mut s).await,
        DaemonMessage::Hello {
            version: PROTOCOL_VERSION
        }
    ));
    assert!(matches!(
        recv_tcp(&mut s).await,
        DaemonMessage::QuotaPush { .. }
    ));
    send_tcp(&mut s, ClientMessage::ListSessions).await;
    assert_eq!(
        recv_tcp(&mut s).await,
        DaemonMessage::SessionList { sessions: vec![] }
    );

    let _ = stop_tx.send(());
}

async fn admit(s: &mut TcpStream, key: &SigningKey) {
    send_tcp(
        s,
        ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            credential: None,
        },
    )
    .await;
    let nonce = match recv_tcp(s).await {
        DaemonMessage::Challenge { nonce } => nonce.into_inner(),
        other => panic!("expected Challenge, got {other:?}"),
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    send_tcp(
        s,
        ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            credential: Some(credential_for(key, &nonce, CREDENTIAL_AUDIENCE, now)),
        },
    )
    .await;
    assert!(matches!(
        recv_tcp(s).await,
        DaemonMessage::Hello {
            version: PROTOCOL_VERSION
        }
    ));
    assert!(matches!(recv_tcp(s).await, DaemonMessage::QuotaPush { .. }));
}

#[tokio::test]
async fn attach_to_another_principals_session_is_rejected() {
    let key_a = random_signing_key();
    let key_b = random_signing_key();
    let allow = Allowlist::from_keys([
        key_a.verifying_key().to_bytes(),
        key_b.verifying_key().to_bytes(),
    ]);
    let daemon = Daemon::new(|| async { vec![] }, |_, _, _| Ok(dummy_pty())).with_allowlist(allow);

    let owner_a = PrincipalId::from_public_key(&key_a.verifying_key().to_bytes());
    let id = SessionId::new("cross-principal-session");
    daemon
        .add_session_owned(
            PathBuf::from("/fake/repo"),
            aihub_git::SessionWorktree {
                session_id: id.clone(),
                path: PathBuf::from("/fake/wt"),
                branch: id.branch_name(),
                base_branch: "main".into(),
                originating_checkout: PathBuf::from("/fake/repo"),
            },
            HarnessId::Codex,
            None,
            owner_a,
        )
        .await
        .unwrap();

    let (addr, stop_tx) = spawn_network_daemon(daemon).await;

    // B never owned this session: attach must be refused, opaquely.
    let mut client_b = TcpStream::connect(addr).await.unwrap();
    admit(&mut client_b, &key_b).await;
    send_tcp(
        &mut client_b,
        ClientMessage::Attach {
            target: SessionTarget::Id(id.clone()),
            last_seen_offset: None,
        },
    )
    .await;
    assert_eq!(recv_tcp(&mut client_b).await, DaemonMessage::Unauthorized);

    // A owns it: attach succeeds normally.
    let mut client_a = TcpStream::connect(addr).await.unwrap();
    admit(&mut client_a, &key_a).await;
    send_tcp(
        &mut client_a,
        ClientMessage::Attach {
            target: SessionTarget::Id(id.clone()),
            last_seen_offset: None,
        },
    )
    .await;
    assert!(matches!(
        recv_tcp(&mut client_a).await,
        DaemonMessage::Attached { .. }
    ));

    let _ = stop_tx.send(());
}

#[tokio::test]
async fn merge_request_over_network_requires_signed_envelope_and_rejects_replayed_nonce() {
    let key = random_signing_key();
    let allow = Allowlist::from_keys([key.verifying_key().to_bytes()]);
    let daemon = Daemon::new(|| async { vec![] }, |_, _, _| Ok(dummy_pty()))
        .with_allowlist(allow)
        .with_git_seams(
            |_, _, _| async { Ok("the diff".to_string()) },
            |_, strategy, _| async move {
                Ok(aihub_git::MergeOutcome {
                    strategy,
                    success: true,
                    diff: "the diff".into(),
                    message: "merged".into(),
                })
            },
        );

    let owner = PrincipalId::from_public_key(&key.verifying_key().to_bytes());
    let id = SessionId::new("merge-envelope-session");
    daemon
        .add_session_owned(
            PathBuf::from("/fake/repo"),
            aihub_git::SessionWorktree {
                session_id: id.clone(),
                path: PathBuf::from("/fake/wt"),
                branch: id.branch_name(),
                base_branch: "main".into(),
                originating_checkout: PathBuf::from("/fake/repo"),
            },
            HarnessId::Codex,
            None,
            owner,
        )
        .await
        .unwrap();

    let (addr, stop_tx) = spawn_network_daemon(daemon).await;
    let mut s = TcpStream::connect(addr).await.unwrap();
    admit(&mut s, &key).await;
    // The final MergeResult is a broadcast scoped to attached clients (unchanged from the
    // pre-Fatia-1 local flow) — attach first, same as the existing local merge tests do.
    send_tcp(
        &mut s,
        ClientMessage::Attach {
            target: SessionTarget::Id(id.clone()),
            last_seen_offset: None,
        },
    )
    .await;
    assert!(matches!(
        recv_tcp(&mut s).await,
        DaemonMessage::Attached { .. }
    ));

    send_tcp(
        &mut s,
        ClientMessage::MergeRequest {
            session_id: id.clone(),
            strategy: MergeStrategy::Squash,
        },
    )
    .await;
    // Preview, same as the local flow, then the destructive-confirmation nonce.
    assert!(matches!(
        recv_tcp(&mut s).await,
        DaemonMessage::MergeResult { success: false, .. }
    ));
    let _first_nonce = match recv_tcp(&mut s).await {
        DaemonMessage::Challenge { nonce } => nonce.into_inner(),
        other => panic!("expected Challenge, got {other:?}"),
    };

    // Repeating the bare MergeRequest (the UDS "repeat to confirm" shape) must NOT finalize
    // a network merge: it only re-issues a fresh preview + a fresh nonce.
    send_tcp(
        &mut s,
        ClientMessage::MergeRequest {
            session_id: id.clone(),
            strategy: MergeStrategy::Squash,
        },
    )
    .await;
    assert!(matches!(
        recv_tcp(&mut s).await,
        DaemonMessage::MergeResult { success: false, .. }
    ));
    let nonce = match recv_tcp(&mut s).await {
        DaemonMessage::Challenge { nonce } => nonce.into_inner(),
        other => panic!("expected Challenge, got {other:?}"),
    };
    assert_ne!(&nonce, &[0u8; 32][..], "nonce must be freshly randomized");

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let envelope = credential_for(&key, &nonce, CREDENTIAL_AUDIENCE, now);
    send_tcp(
        &mut s,
        ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            credential: Some(envelope.clone()),
        },
    )
    .await;
    assert_eq!(
        recv_tcp(&mut s).await,
        DaemonMessage::MergeResult {
            session_id: id.clone(),
            success: true,
            diff: "the diff".into(),
            message: "merged".into(),
        }
    );

    // Replay: the exact same signed envelope, over the same nonce, resent. The nonce was
    // already consumed — this must never finalize a second merge.
    send_tcp(
        &mut s,
        ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            credential: Some(envelope),
        },
    )
    .await;
    let replay_response = recv_tcp(&mut s).await;
    assert!(
        !matches!(
            replay_response,
            DaemonMessage::MergeResult { success: true, .. }
        ),
        "replayed nonce must not finalize a second merge, got {replay_response:?}"
    );

    let _ = stop_tx.send(());
}

#[tokio::test]
async fn audit_log_records_real_reason_without_leaking_the_credential() {
    let audit_path = test_dir().join("audit.jsonl");
    let daemon = Daemon::new(|| async { vec![] }, |_, _, _| panic!("no spawn"))
        .with_allowlist(Allowlist::empty())
        .with_audit_log_path(audit_path.clone());
    let (addr, stop_tx) = spawn_network_daemon(daemon).await;

    let key = random_signing_key();
    let mut s = TcpStream::connect(addr).await.unwrap();
    send_tcp(
        &mut s,
        ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            credential: None,
        },
    )
    .await;
    let nonce = match recv_tcp(&mut s).await {
        DaemonMessage::Challenge { nonce } => nonce.into_inner(),
        other => panic!("expected Challenge, got {other:?}"),
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    // Correctly signed, but the key was never approved by the owner (empty allowlist).
    let credential = credential_for(&key, &nonce, CREDENTIAL_AUDIENCE, now);
    let leaked_pubkey_b64 = {
        use aihub_core::Base64Bytes;
        serde_json::to_string(&Base64Bytes::new(key.verifying_key().to_bytes().to_vec())).unwrap()
    };
    send_tcp(
        &mut s,
        ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            credential: Some(credential),
        },
    )
    .await;
    assert_eq!(recv_tcp(&mut s).await, DaemonMessage::Unauthorized);

    // Give the audit write a moment; it happens before the wire reply so this is generous.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let contents = std::fs::read_to_string(&audit_path).unwrap();
    assert!(
        contents.contains("allowlist"),
        "audit log must record the real rejection reason: {contents}"
    );
    assert!(
        !contents.contains(leaked_pubkey_b64.trim_matches('"')),
        "audit log must never contain the raw credential key material: {contents}"
    );

    let _ = stop_tx.send(());
}
