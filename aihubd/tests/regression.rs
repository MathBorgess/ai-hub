use aihub_core::*;
use aihubd::{log_lifecycle, Daemon, Pty};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    sync::broadcast,
};

static SOCKET_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1000);

fn test_socket() -> PathBuf {
    std::env::temp_dir()
        .join(format!(
            "ah08-reg-{}-{}-{}",
            std::process::id(),
            SOCKET_COUNTER.fetch_add(1, Ordering::SeqCst),
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
    tokio::time::timeout(Duration::from_secs(30), async {
        let n = s.read_u32().await.unwrap();
        let mut data = vec![0; n as usize];
        s.read_exact(&mut data).await.unwrap();
        match serde_json::from_slice::<IpcMessage>(&data).unwrap() {
            IpcMessage::Daemon(m) => m,
            _ => panic!("wrong direction in IPC frame"),
        }
    })
    .await
    .unwrap()
}

async fn connect_and_handshake(path: &std::path::Path) -> UnixStream {
    let mut s = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(stream) = UnixStream::connect(path).await {
                break stream;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("daemon must bind socket within 3s");

    send(
        &mut s,
        ClientMessage::Hello {
            version: PROTOCOL_VERSION,
        },
    )
    .await;
    assert!(matches!(
        recv(&mut s).await,
        DaemonMessage::Hello { version } if version == PROTOCOL_VERSION
    ));
    assert!(matches!(
        recv(&mut s).await,
        DaemonMessage::QuotaPush { .. }
    ));
    s
}

async fn attach_session(client: &mut UnixStream, session_id: &SessionId) {
    send(
        client,
        ClientMessage::Attach {
            target: SessionTarget::Id(session_id.clone()),
        },
    )
    .await;
    match recv(client).await {
        DaemonMessage::Attached {
            session_id: attached,
            ..
        } => assert_eq!(attached, *session_id),
        other => panic!("expected Attached, got {other:?}"),
    }
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

// ---------------------------------------------------------------------------
// Finding F4: Stop barrier reap before final diff & finish, and on shutdown
// ---------------------------------------------------------------------------

#[tokio::test]
async fn f4_merge_ordering_through_stop_barrier() {
    let path = test_socket();
    let events = Arc::new(Mutex::new(Vec::<String>::new()));

    let stop_events = events.clone();
    let diff_events = events.clone();
    let finish_events = events.clone();

    let daemon = Daemon::new(
        || async { vec![] },
        move |_, _| {
            let mut pty = dummy_pty();
            let stop_events = stop_events.clone();
            pty.stop = Arc::new(move |_| {
                stop_events.lock().unwrap().push("stop_barrier".into());
                Box::pin(async { Ok(Some(0)) })
            });
            Ok(pty)
        },
    )
    .with_git_seams(
        move |_, _, _| {
            let diff_events = diff_events.clone();
            async move {
                diff_events.lock().unwrap().push("diff".into());
                Ok("fake diff content".to_string())
            }
        },
        move |_, strategy, _| {
            let finish_events = finish_events.clone();
            async move {
                finish_events.lock().unwrap().push("finish".into());
                Ok(aihub_git::MergeOutcome {
                    strategy,
                    success: true,
                    diff: "fake diff content".into(),
                    message: "Squashed cleanly".into(),
                })
            }
        },
    );

    let id = SessionId::new("f4-merge-session");
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

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let p = path.clone();
    let d = daemon.clone();
    let daemon_task = tokio::spawn(async move {
        d.run(p, async {
            stop_rx.await.ok();
        })
        .await
    });

    let mut client = connect_and_handshake(&path).await;
    attach_session(&mut client, &id).await;

    // First MergeRequest: preview diff
    send(
        &mut client,
        ClientMessage::MergeRequest {
            session_id: id.clone(),
            strategy: MergeStrategy::Squash,
        },
    )
    .await;

    let preview = recv(&mut client).await;
    match preview {
        DaemonMessage::MergeResult { success, diff, .. } => {
            assert!(!success, "first merge request must only be a preview");
            assert_eq!(diff, "fake diff content");
        }
        other => panic!("expected MergeResult, got {other:?}"),
    }
    assert_eq!(*events.lock().unwrap(), vec!["diff"]);

    // Second MergeRequest: confirmation -> stop barrier -> stopped diff -> finish
    send(
        &mut client,
        ClientMessage::MergeRequest {
            session_id: id.clone(),
            strategy: MergeStrategy::Squash,
        },
    )
    .await;

    let finish_msg = recv(&mut client).await;
    match finish_msg {
        DaemonMessage::MergeResult { success, .. } => {
            assert!(success, "second merge request must confirm merge");
        }
        other => panic!("expected MergeResult, got {other:?}"),
    }

    // Exact ordering: diff (preview) -> diff (check confirmation) -> stop_barrier -> diff (reaped diff) -> finish
    let recorded = events.lock().unwrap().clone();
    assert_eq!(
        recorded,
        vec!["diff", "diff", "stop_barrier", "diff", "finish"],
        "stop barrier must reap child before final diff and finish"
    );

    stop_tx.send(()).unwrap();
    daemon_task.await.unwrap().unwrap();
    if let Some(parent) = path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[tokio::test]
async fn f4_shutdown_reaps_through_stop_barrier() {
    let path = test_socket();
    let reaped = Arc::new(AtomicUsize::new(0));
    let reaped_clone = reaped.clone();

    let daemon = Daemon::new(
        || async { vec![] },
        move |_, _| {
            let mut pty = dummy_pty();
            let reaped = reaped_clone.clone();
            pty.stop = Arc::new(move |_| {
                reaped.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(Some(0)) })
            });
            Ok(pty)
        },
    );

    let id = SessionId::new("f4-shutdown-session");
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

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let p = path.clone();
    let daemon_task = tokio::spawn(async move {
        daemon
            .run(p, async {
                stop_rx.await.ok();
            })
            .await
    });

    let _client = connect_and_handshake(&path).await;
    assert_eq!(reaped.load(Ordering::SeqCst), 0);

    stop_tx.send(()).unwrap();
    daemon_task.await.unwrap().unwrap();

    assert_eq!(
        reaped.load(Ordering::SeqCst),
        1,
        "shutdown must reap child through the stop barrier"
    );

    if let Some(parent) = path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

// ---------------------------------------------------------------------------
// Finding F5: Switch ordering (no overlap) and recoverable stopped state
// ---------------------------------------------------------------------------

#[tokio::test]
async fn f5_switch_ordering_no_overlap() {
    let path = test_socket();
    let events = Arc::new(Mutex::new(Vec::<String>::new()));
    let outgoing_running = Arc::new(AtomicBool::new(true));

    let events_stop = events.clone();
    let events_ext = events.clone();
    let events_spawn = events.clone();

    let running_stop = outgoing_running.clone();
    let running_ext = outgoing_running.clone();
    let running_spawn = outgoing_running.clone();

    let daemon = Daemon::new(
        || async { vec![] },
        move |harness, _| {
            let mut pty = dummy_pty();
            if harness == HarnessId::Codex {
                let events = events_stop.clone();
                let running = running_stop.clone();
                pty.stop = Arc::new(move |_| {
                    events.lock().unwrap().push("stop_barrier".into());
                    running.store(false, Ordering::SeqCst);
                    Box::pin(async { Ok(Some(0)) })
                });
            } else {
                let events = events_spawn.clone();
                let running = running_spawn.clone();
                assert!(
                    !running.load(Ordering::SeqCst),
                    "incoming harness must not start while outgoing harness is running"
                );
                events.lock().unwrap().push("launch".into());
            }
            Ok(pty)
        },
    )
    .with_memory_extractor(move |_, _, _| {
        let events = events_ext.clone();
        let running = running_ext.clone();
        async move {
            assert!(
                !running.load(Ordering::SeqCst),
                "extraction must occur only after outgoing harness is stopped"
            );
            events.lock().unwrap().push("extract".into());
            Ok(aihub_memory::HandoffTurn {
                summary: "done turn".into(),
                last_output: "output".into(),
                decisions: vec![],
            })
        }
    })
    .with_memory_recorder(|_, _, _, _| async {
        Ok(aihub_memory::HandoffDestination::SpooledLocally)
    });

    let temp_wt = std::env::temp_dir().join(format!("ah08-wt-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&temp_wt);

    let id = SessionId::new("f5-switch-session");
    daemon
        .add_session(
            PathBuf::from("/fake/repo"),
            aihub_git::SessionWorktree {
                session_id: id.clone(),
                path: temp_wt.clone(),
                branch: id.branch_name(),
                base_branch: "main".into(),
                originating_checkout: PathBuf::from("/fake/repo"),
            },
            HarnessId::Codex,
            None,
        )
        .await
        .unwrap();

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let p = path.clone();
    let daemon_task = tokio::spawn(async move {
        daemon
            .run(p, async {
                stop_rx.await.ok();
            })
            .await
    });

    let mut client = connect_and_handshake(&path).await;
    attach_session(&mut client, &id).await;

    send(
        &mut client,
        ClientMessage::SwitchHarness {
            session_id: id.clone(),
            target: HarnessId::ClaudeCode,
            with_handoff: true,
        },
    )
    .await;

    match recv(&mut client).await {
        DaemonMessage::HarnessSwitched {
            session_id,
            old_harness,
            new_harness,
            ..
        } => {
            assert_eq!(session_id, id);
            assert_eq!(old_harness, HarnessId::Codex);
            assert_eq!(new_harness, HarnessId::ClaudeCode);
        }
        other => panic!("expected HarnessSwitched, got {other:?}"),
    }

    assert_eq!(
        *events.lock().unwrap(),
        vec!["stop_barrier", "extract", "launch"],
        "switch must follow stop -> extract -> launch order without overlap"
    );

    stop_tx.send(()).unwrap();
    daemon_task.await.unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&temp_wt);
    if let Some(parent) = path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[tokio::test]
async fn f5_switch_recoverable_stopped_state_on_spawn_failure() {
    let path = test_socket();

    let daemon = Daemon::new(
        || async { vec![] },
        |harness, _| {
            if harness == HarnessId::Codex {
                Ok(dummy_pty())
            } else {
                anyhow::bail!("incoming harness binary execution failed")
            }
        },
    );

    let id = SessionId::new("f5-recoverable-session");
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

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let p = path.clone();
    let daemon_task = tokio::spawn(async move {
        daemon
            .run(p, async {
                stop_rx.await.ok();
            })
            .await
    });

    let mut client = connect_and_handshake(&path).await;
    attach_session(&mut client, &id).await;

    send(
        &mut client,
        ClientMessage::SwitchHarness {
            session_id: id.clone(),
            target: HarnessId::ClaudeCode,
            with_handoff: false,
        },
    )
    .await;

    // Broadcasts SessionExited and sends request Error
    let msg1 = recv(&mut client).await;
    let msg2 = recv(&mut client).await;
    assert!(
        (matches!(msg1, DaemonMessage::SessionExited { .. })
            && matches!(msg2, DaemonMessage::Error { .. }))
            || (matches!(msg2, DaemonMessage::SessionExited { .. })
                && matches!(msg1, DaemonMessage::Error { .. }))
    );

    // Session is kept in registry in recoverable stopped state
    send(&mut client, ClientMessage::ListSessions).await;
    match recv(&mut client).await {
        DaemonMessage::SessionList { sessions } => {
            assert_eq!(sessions.len(), 1);
            assert_eq!(sessions[0].session_id, id);
            assert!(!sessions[0].active, "session must be stopped but preserved");
            assert_eq!(sessions[0].worktree_path, PathBuf::from("/fake/wt"));
        }
        other => panic!("expected SessionList, got {other:?}"),
    }

    stop_tx.send(()).unwrap();
    daemon_task.await.unwrap().unwrap();
    if let Some(parent) = path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

// ---------------------------------------------------------------------------
// Finding F7: Registry lock released before PTY I/O, queue full error isolated
// ---------------------------------------------------------------------------

#[tokio::test]
async fn f7_lock_released_before_pty_io_and_full_queue_isolated() {
    let path = test_socket();

    let daemon = Daemon::new(
        || async { vec![] },
        |_, _| {
            let mut pty = dummy_pty();
            pty.try_write = Arc::new(|_| anyhow::bail!("PTY input queue full"));
            Ok(pty)
        },
    );

    let id = SessionId::new("f7-session");
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

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let p = path.clone();
    let daemon_task = tokio::spawn(async move {
        daemon
            .run(p, async {
                stop_rx.await.ok();
            })
            .await
    });

    let mut client_a = connect_and_handshake(&path).await;
    let mut client_b = connect_and_handshake(&path).await;

    send(
        &mut client_a,
        ClientMessage::PtyInput {
            session_id: id.clone(),
            data: b"input bytes".to_vec().into(),
        },
    )
    .await;

    // Client A receives error due to full queue
    match recv(&mut client_a).await {
        DaemonMessage::Error { code, message } => {
            assert_eq!(code, "pty_input_error");
            assert!(message.contains("PTY input queue full"));
        }
        other => panic!("expected Error, got {other:?}"),
    }

    // Client B can immediately query sessions without being blocked
    send(&mut client_b, ClientMessage::ListSessions).await;
    match recv(&mut client_b).await {
        DaemonMessage::SessionList { sessions } => {
            assert_eq!(sessions.len(), 1);
        }
        other => panic!("expected SessionList, got {other:?}"),
    }

    stop_tx.send(()).unwrap();
    daemon_task.await.unwrap().unwrap();
    if let Some(parent) = path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

// ---------------------------------------------------------------------------
// Finding F9: SubmitTask sets routing context and calls classify_with_fallback
// ---------------------------------------------------------------------------

#[tokio::test]
async fn f9_submit_task_context_and_classify_fallback() {
    let path = test_socket();
    let classified_prompt = Arc::new(Mutex::new(None));
    let cp = classified_prompt.clone();

    let daemon = Daemon::new(|| async { vec![] }, |_, _| Ok(dummy_pty()))
        .with_classifier(move |prompt| {
            let cp = cp.clone();
            let p = prompt.to_string();
            async move {
                *cp.lock().unwrap() = Some(p);
                aihub_router::Classification {
                    tier: TaskTier::Review,
                    confidence: 0.9,
                    ambiguous: false,
                }
            }
        })
        .with_router(|tier, size, _, _| {
            assert_eq!(tier, TaskTier::Review);
            assert_eq!(size, TaskSize::M);
            Ok(RouteOutcome::Recommendation {
                harness: HarnessId::Codex,
                lane: None,
                model: None,
                holds_until_s: None,
            })
        });

    let id = SessionId::new("f9-session");
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
            HarnessId::ClaudeCode,
            None,
        )
        .await
        .unwrap();

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let p = path.clone();
    let daemon_task = tokio::spawn(async move {
        daemon
            .run(p, async {
                stop_rx.await.ok();
            })
            .await
    });

    let mut client = connect_and_handshake(&path).await;
    attach_session(&mut client, &id).await;

    send(
        &mut client,
        ClientMessage::SubmitTask {
            session_id: id.clone(),
            task: "audit memory system for race conditions".into(),
        },
    )
    .await;

    match recv(&mut client).await {
        DaemonMessage::RouteRecommendation {
            session_id,
            outcome,
        } => {
            assert_eq!(session_id, id);
            assert!(matches!(outcome, RouteOutcome::Recommendation { .. }));
        }
        other => panic!("expected RouteRecommendation, got {other:?}"),
    }

    assert_eq!(
        classified_prompt.lock().unwrap().as_deref(),
        Some("audit memory system for race conditions")
    );

    stop_tx.send(()).unwrap();
    daemon_task.await.unwrap().unwrap();
    if let Some(parent) = path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

// ---------------------------------------------------------------------------
// Finding F10: NoCapacity outcome is never dispatched in autonomous mode
// ---------------------------------------------------------------------------

#[tokio::test]
async fn f10_no_capacity_never_dispatched() {
    let path = test_socket();

    let daemon =
        Daemon::new(|| async { vec![] }, |_, _| Ok(dummy_pty())).with_router(|_, _, _, _| {
            Ok(RouteOutcome::NoCapacity {
                reason: "all provider windows exhausted".into(),
            })
        });

    let id = SessionId::new("f10-session");
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

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let p = path.clone();
    let daemon_task = tokio::spawn(async move {
        daemon
            .run(p, async {
                stop_rx.await.ok();
            })
            .await
    });

    let mut client = connect_and_handshake(&path).await;
    attach_session(&mut client, &id).await;

    // Set autonomous mode
    send(
        &mut client,
        ClientMessage::SetMode {
            session_id: id.clone(),
            mode: Mode::Autonomous,
        },
    )
    .await;
    assert!(matches!(
        recv(&mut client).await,
        DaemonMessage::ModeSet { .. }
    ));

    // Submit task with NoCapacity outcome
    send(
        &mut client,
        ClientMessage::SubmitTask {
            session_id: id.clone(),
            task: "autonomous task".into(),
        },
    )
    .await;

    match recv(&mut client).await {
        DaemonMessage::RouteRecommendation {
            session_id,
            outcome,
        } => {
            assert_eq!(session_id, id);
            match outcome {
                RouteOutcome::NoCapacity { reason } => {
                    assert_eq!(reason, "all provider windows exhausted");
                }
                other => panic!("expected NoCapacity, got {other:?}"),
            }
        }
        other => panic!("expected RouteRecommendation, got {other:?}"),
    }

    // Verify session was NOT switched
    send(&mut client, ClientMessage::ListSessions).await;
    match recv(&mut client).await {
        DaemonMessage::SessionList { sessions } => {
            assert_eq!(sessions.len(), 1);
            assert_eq!(sessions[0].harness, HarnessId::Codex);
            assert!(sessions[0].active);
        }
        other => panic!("expected SessionList, got {other:?}"),
    }

    stop_tx.send(()).unwrap();
    daemon_task.await.unwrap().unwrap();
    if let Some(parent) = path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

// ---------------------------------------------------------------------------
// Finding F13: Session-scoped events name session_id and attach carries summary
// ---------------------------------------------------------------------------

#[tokio::test]
async fn f13_session_scoped_events_and_attach_summary() {
    let path = test_socket();

    let daemon = Daemon::new(|| async { vec![] }, |_, _| Ok(dummy_pty()));

    let id1 = SessionId::new("f13-session-1");
    let id2 = SessionId::new("f13-session-2");

    daemon
        .add_session(
            PathBuf::from("/fake/repo1"),
            aihub_git::SessionWorktree {
                session_id: id1.clone(),
                path: PathBuf::from("/fake/wt1"),
                branch: id1.branch_name(),
                base_branch: "main".into(),
                originating_checkout: PathBuf::from("/fake/repo1"),
            },
            HarnessId::Codex,
            None,
        )
        .await
        .unwrap();

    daemon
        .add_session(
            PathBuf::from("/fake/repo2"),
            aihub_git::SessionWorktree {
                session_id: id2.clone(),
                path: PathBuf::from("/fake/wt2"),
                branch: id2.branch_name(),
                base_branch: "main".into(),
                originating_checkout: PathBuf::from("/fake/repo2"),
            },
            HarnessId::ClaudeCode,
            None,
        )
        .await
        .unwrap();

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let p = path.clone();
    let daemon_task = tokio::spawn(async move {
        daemon
            .run(p, async {
                stop_rx.await.ok();
            })
            .await
    });

    let mut client = connect_and_handshake(&path).await;

    // Attach to session 1 carries SessionSummary
    send(
        &mut client,
        ClientMessage::Attach {
            target: SessionTarget::Id(id1.clone()),
        },
    )
    .await;

    match recv(&mut client).await {
        DaemonMessage::Attached {
            session_id,
            summary,
            ..
        } => {
            assert_eq!(session_id, id1);
            assert_eq!(summary.session_id, id1);
            assert_eq!(summary.harness, HarnessId::Codex);
            assert_eq!(summary.repo_path, PathBuf::from("/fake/repo1"));
            assert!(summary.active);
        }
        other => panic!("expected Attached, got {other:?}"),
    }

    // SetMode names session_id
    send(
        &mut client,
        ClientMessage::SetMode {
            session_id: id1.clone(),
            mode: Mode::Autonomous,
        },
    )
    .await;
    match recv(&mut client).await {
        DaemonMessage::ModeSet { session_id, mode } => {
            assert_eq!(session_id, id1);
            assert_eq!(mode, Mode::Autonomous);
        }
        other => panic!("expected ModeSet, got {other:?}"),
    }

    // SubmitTask route recommendation names session_id
    send(
        &mut client,
        ClientMessage::SubmitTask {
            session_id: id2.clone(),
            task: "test task for session 2".into(),
        },
    )
    .await;
    match recv(&mut client).await {
        DaemonMessage::RouteRecommendation { session_id, .. } => {
            assert_eq!(session_id, id2);
        }
        other => panic!("expected RouteRecommendation, got {other:?}"),
    }

    // SwitchHarness names session_id
    send(
        &mut client,
        ClientMessage::SwitchHarness {
            session_id: id1.clone(),
            target: HarnessId::ClaudeCode,
            with_handoff: false,
        },
    )
    .await;
    match recv(&mut client).await {
        DaemonMessage::HarnessSwitched {
            session_id,
            old_harness,
            new_harness,
            ..
        } => {
            assert_eq!(session_id, id1);
            assert_eq!(old_harness, HarnessId::Codex);
            assert_eq!(new_harness, HarnessId::ClaudeCode);
        }
        other => panic!("expected HarnessSwitched, got {other:?}"),
    }

    stop_tx.send(()).unwrap();
    daemon_task.await.unwrap().unwrap();
    if let Some(parent) = path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

// ---------------------------------------------------------------------------
// Socket chmod error handling, lifecycle logging, and handoff delivery status
// ---------------------------------------------------------------------------

#[tokio::test]
async fn socket_chmod_failure_detected() {
    // Binding to a path inside a forbidden or non-directory location must fail
    let bad_path = PathBuf::from("/dev/null/aihub-forbidden.sock");
    let daemon = Daemon::new(|| async { vec![] }, |_, _| Ok(dummy_pty()));
    let result = daemon.run(bad_path, std::future::pending()).await;
    assert!(
        result.is_err(),
        "daemon must fail when socket cannot be created"
    );
}

#[tokio::test]
async fn lifecycle_logging_and_handoff_destination() {
    // Test lifecycle logging helper outputs cleanly
    log_lifecycle("INFO", "start", "testing start logging");
    log_lifecycle("INFO", "bind", "testing bind logging");
    log_lifecycle("INFO", "session spawn", "testing spawn logging");
    log_lifecycle("INFO", "session stop", "testing stop logging");
    log_lifecycle("INFO", "session switch", "testing switch logging");
    log_lifecycle("INFO", "session merge", "testing merge logging");
    log_lifecycle("INFO", "probe refresh", "testing probe logging");
    log_lifecycle("ERROR", "test error", "testing error logging");

    // Test handoff delivery destination reporting through switch_session
    let temp_wt = std::env::temp_dir().join(format!("ah08-wt-dest-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&temp_wt);

    let daemon_aimem = Daemon::new(|| async { vec![] }, |_, _| Ok(dummy_pty()))
        .with_memory_extractor(|_, _, _| async {
            Ok(aihub_memory::HandoffTurn {
                summary: "summary".into(),
                last_output: "output".into(),
                decisions: vec![],
            })
        })
        .with_memory_recorder(|_, _, _, _| async {
            Ok(aihub_memory::HandoffDestination::AiMemory)
        });

    let id = SessionId::new("dest-session-1");
    daemon_aimem
        .add_session(
            PathBuf::from("/fake/repo"),
            aihub_git::SessionWorktree {
                session_id: id.clone(),
                path: temp_wt.clone(),
                branch: id.branch_name(),
                base_branch: "main".into(),
                originating_checkout: PathBuf::from("/fake/repo"),
            },
            HarnessId::Codex,
            None,
        )
        .await
        .unwrap();

    let dest = daemon_aimem
        .switch_session(&id, HarnessId::ClaudeCode, true)
        .await
        .unwrap();
    assert_eq!(dest, Some(aihub_memory::HandoffDestination::AiMemory));

    let daemon_spool = Daemon::new(|| async { vec![] }, |_, _| Ok(dummy_pty()))
        .with_memory_extractor(|_, _, _| async {
            Ok(aihub_memory::HandoffTurn {
                summary: "summary".into(),
                last_output: "output".into(),
                decisions: vec![],
            })
        })
        .with_memory_recorder(|_, _, _, _| async {
            Ok(aihub_memory::HandoffDestination::SpooledLocally)
        });

    let id2 = SessionId::new("dest-session-2");
    daemon_spool
        .add_session(
            PathBuf::from("/fake/repo"),
            aihub_git::SessionWorktree {
                session_id: id2.clone(),
                path: temp_wt.clone(),
                branch: id2.branch_name(),
                base_branch: "main".into(),
                originating_checkout: PathBuf::from("/fake/repo"),
            },
            HarnessId::Codex,
            None,
        )
        .await
        .unwrap();

    let dest2 = daemon_spool
        .switch_session(&id2, HarnessId::ClaudeCode, true)
        .await
        .unwrap();
    assert_eq!(
        dest2,
        Some(aihub_memory::HandoffDestination::SpooledLocally)
    );

    let _ = std::fs::remove_dir_all(&temp_wt);
}
