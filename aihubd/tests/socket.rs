use aihub_core::*;
use std::{path::PathBuf, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
};

static SOCKET_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn socket() -> PathBuf {
    std::env::temp_dir()
        .join(format!(
            "ah08-{}-{}-{}",
            std::process::id(),
            SOCKET_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
        .join("s")
}
async fn send(s: &mut UnixStream, msg: ClientMessage) {
    s.write_all(&encode_frame(&msg.into()).unwrap())
        .await
        .unwrap();
}
async fn recv(s: &mut UnixStream) -> DaemonMessage {
    tokio::time::timeout(Duration::from_secs(3), async {
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
#[tokio::test]
async fn binary_refuses_live_socket_without_probing() {
    let path = socket();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let _listener = tokio::net::UnixListener::bind(&path).unwrap();
    let result = tokio::process::Command::new(env!("CARGO_BIN_EXE_aihubd"))
        .arg("--socket")
        .arg(&path)
        .output()
        .await
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("live aihubd"));
    assert!(path.exists());
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

// PTY and probe are injected; no credential files, harnesses or network are used.
#[tokio::test]
async fn cached_quota_broadcast_attach_and_disconnect() {
    use aihubd::{Daemon, Pty};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    let path = socket();
    let calls = Arc::new(AtomicUsize::new(0));
    let killed = Arc::new(AtomicUsize::new(0));
    let (output, _) = tokio::sync::broadcast::channel(16);
    let probe_calls = calls.clone();
    let out = output.clone();
    let dead = killed.clone();
    let daemon = Daemon::new(
        move || {
            let n = probe_calls.fetch_add(1, Ordering::SeqCst);
            async move {
                vec![QuotaSnapshot {
                    slot: SlotId::default_for(HarnessId::Codex),
                    status: QuotaStatus::Unknown,
                    source: QuotaSource::Vendor,
                    estimated: false,
                    note: Some(format!("sample {n}")),
                    windows: vec![],
                    lanes: vec![],
                }]
            }
        },
        move |_, _| {
            let dead1 = dead.clone();
            let dead2 = dead.clone();
            Ok(Pty {
                output: out.subscribe(),
                scrollback: b"before attach".to_vec(),
                write: Box::new(|_| Box::pin(async { Ok(()) })),
                resize: Box::new(|_| Ok(())),
                wait: Box::new(|| Box::pin(std::future::pending())),
                kill: Box::new(move || {
                    let dead = dead1.clone();
                    Box::pin(async move {
                        dead.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    })
                }),
                stop: Arc::new(move |_| {
                    let dead = dead2.clone();
                    Box::pin(async move {
                        dead.fetch_add(1, Ordering::SeqCst);
                        Ok(Some(0))
                    })
                }),
                try_write: Arc::new(|_| Ok(())),
            })
        },
    );
    let id = SessionId::new("test");
    daemon
        .add_session(
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
        )
        .await
        .unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let p = path.clone();
    let task = tokio::spawn(async move {
        daemon
            .run(p, async {
                stopped.await.ok();
            })
            .await
    });
    let mut a = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(s) = UnixStream::connect(&path).await {
                break s;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("daemon must bind a socket within 3 seconds");
    send(
        &mut a,
        ClientMessage::Hello {
            version: PROTOCOL_VERSION,
        },
    )
    .await;
    assert!(matches!(
        recv(&mut a).await,
        DaemonMessage::Hello { version } if version == PROTOCOL_VERSION
    ));
    let snap = loop {
        match recv(&mut a).await {
            DaemonMessage::QuotaPush { snapshots } if !snapshots.is_empty() => break snapshots,
            DaemonMessage::QuotaPush { .. } => (),
            other => panic!("{other:?}"),
        }
    };
    let mut b = UnixStream::connect(&path).await.unwrap();
    send(
        &mut b,
        ClientMessage::Hello {
            version: PROTOCOL_VERSION,
        },
    )
    .await;
    assert!(matches!(recv(&mut b).await, DaemonMessage::Hello { .. }));
    match recv(&mut b).await {
        DaemonMessage::QuotaPush { snapshots } => assert!(!snapshots.is_empty()),
        other => panic!("{other:?}"),
    }
    assert!(!snap.is_empty());
    send(&mut b, ClientMessage::ListSessions).await;
    match recv(&mut b).await {
        DaemonMessage::SessionList { sessions } => {
            assert_eq!(sessions.len(), 1);
            assert!(sessions[0].active);
        }
        other => panic!("{other:?}"),
    }
    send(
        &mut b,
        ClientMessage::Attach {
            target: SessionTarget::Id(id.clone()),
        },
    )
    .await;
    match recv(&mut b).await {
        DaemonMessage::Attached {
            session_id,
            scrollback,
            ..
        } => {
            assert_eq!(session_id, id.clone());
            assert_eq!(scrollback, b"before attach".to_vec().into());
        }
        other => panic!("expected Attached, got {other:?}"),
    }
    output.send(b"live".to_vec()).unwrap();
    assert_eq!(
        recv(&mut b).await,
        DaemonMessage::PtyOutput {
            session_id: id.clone(),
            data: b"live".to_vec().into()
        }
    );
    drop(b);
    assert_eq!(killed.load(Ordering::SeqCst), 0);
    let mut c = UnixStream::connect(&path).await.unwrap();
    send(
        &mut c,
        ClientMessage::Hello {
            version: PROTOCOL_VERSION,
        },
    )
    .await;
    recv(&mut c).await;
    recv(&mut c).await;
    send(
        &mut c,
        ClientMessage::Attach {
            target: SessionTarget::LatestForRepo(PathBuf::from("/fake/repo")),
        },
    )
    .await;
    match recv(&mut c).await {
        DaemonMessage::Attached {
            session_id,
            scrollback,
            ..
        } => {
            assert_eq!(session_id, id.clone());
            assert_eq!(scrollback, b"before attachlive".to_vec().into());
        }
        other => panic!("expected Attached, got {other:?}"),
    }
    send(
        &mut c,
        ClientMessage::SetMode {
            session_id: id.clone(),
            mode: Mode::Autonomous,
        },
    )
    .await;
    assert!(matches!(recv(&mut c).await, DaemonMessage::ModeSet { .. }));
    send(&mut c, ClientMessage::RequestQuota).await;
    assert!(matches!(
        recv(&mut c).await,
        DaemonMessage::QuotaPush { .. }
    ));
    assert!(matches!(
        recv(&mut a).await,
        DaemonMessage::QuotaPush { .. }
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
    assert!(!path.exists());
    assert_eq!(killed.load(Ordering::SeqCst), 1);
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[tokio::test]
async fn stale_socket_permissions_and_shutdown() {
    use std::os::unix::fs::PermissionsExt;
    let path = socket();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
    let daemon = aihubd::Daemon::new(|| async { vec![] }, |_, _| panic!("no PTY requested"));
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let p = path.clone();
    let task = tokio::spawn(async move {
        daemon
            .run(p, async {
                stopped.await.ok();
            })
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if UnixStream::connect(&path).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    let other = aihubd::Daemon::new(
        || async { panic!("must refuse before probe") },
        |_, _| panic!("no PTY requested"),
    );
    assert!(other
        .run(path.clone(), std::future::pending())
        .await
        .unwrap_err()
        .to_string()
        .contains("live aihubd"));
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
    assert!(!path.exists());
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

// Re-exec the test harness with fake dependencies to exercise real signals.
// The production binary has no environment switch that disables real probing.
#[tokio::test]
async fn signal_worker() {
    let Some(path) = std::env::var_os("AIHUBD_TEST_SOCKET") else {
        return;
    };
    aihubd::Daemon::new(|| async { vec![] }, |_, _| panic!("no PTY requested"))
        .run(path.into(), aihubd::shutdown_signal())
        .await
        .unwrap();
}

#[tokio::test]
async fn sigterm_and_sigint_remove_socket() {
    for signal in ["-TERM", "-INT"] {
        let path = socket();
        let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "signal_worker", "--nocapture"])
            .env("AIHUBD_TEST_SOCKET", &path)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if UnixStream::connect(&path).await.is_ok() {
                    break;
                }
                assert!(
                    child.try_wait().unwrap().is_none(),
                    "signal worker failed to start"
                );
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let status = tokio::process::Command::new("/bin/kill")
            .arg(signal)
            .arg(child.id().unwrap().to_string())
            .status()
            .await
            .unwrap();
        assert!(status.success());
        assert!(tokio::time::timeout(Duration::from_secs(3), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success());
        assert!(!path.exists());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
