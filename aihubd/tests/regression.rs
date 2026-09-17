use aihub_core::*;
use aihubd::{log_lifecycle, Daemon, Pty};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
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

async fn recv_raw(s: &mut UnixStream) -> DaemonMessage {
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

/// QuotaPush is an unscoped push the daemon may interleave with any reply
/// (its background probe loop broadcasts on its first tick). Callers waiting
/// on a specific reply must skip it rather than assume strict request/response
/// ordering; bounded by recv_raw's own 30s timeout per read.
async fn recv(s: &mut UnixStream) -> DaemonMessage {
    loop {
        let msg = recv_raw(s).await;
        if !matches!(msg, DaemonMessage::QuotaPush { .. }) {
            return msg;
        }
    }
}

/// Operation replies can arrive after `SessionExited` or `SessionList` broadcasts
/// (for example when `wait_session_stopped` polled the registry just before merge).
async fn recv_operation_reply(s: &mut UnixStream) -> DaemonMessage {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match recv(s).await {
                DaemonMessage::SessionExited { .. } | DaemonMessage::SessionList { .. } => {}
                msg => return msg,
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for daemon operation reply"))
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
            credential: None,
        },
    )
    .await;
    assert!(matches!(
        recv_raw(&mut s).await,
        DaemonMessage::Hello { version } if version == PROTOCOL_VERSION
    ));
    assert!(matches!(
        recv_raw(&mut s).await,
        DaemonMessage::QuotaPush { .. }
    ));
    s
}

async fn attach_session(client: &mut UnixStream, session_id: &SessionId) {
    send(
        client,
        ClientMessage::Attach {
            target: SessionTarget::Id(session_id.clone()),
            last_seen_offset: None,
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

/// Stop fails for the first `failures` calls, then succeeds (shutdown retries uncertain groups).
fn failing_stop_for_n_attempts(failures: usize) -> (Pty, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut pty = dummy_pty();
    pty.stop = Arc::new({
        let calls = calls.clone();
        move |_| {
            let calls = calls.clone();
            Box::pin(async move {
                if calls.fetch_add(1, Ordering::SeqCst) < failures {
                    anyhow::bail!("stop barrier signal failed")
                } else {
                    Ok(Some(0))
                }
            })
        }
    });
    (pty, calls)
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
        move |_, _, _| {
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
        move |_, _, _| {
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
        move |harness, _, _| {
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
    .with_memory_recorder(|_, _, _, _, _| async { Ok(aihub_memory::HandoffDestination::Spooled) });

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
            model: None,
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
        |harness, _, _| {
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
            model: None,
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
        |_, _, _| {
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

    let daemon = Daemon::new(|| async { vec![] }, |_, _, _| Ok(dummy_pty()))
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
        .with_router(|tier, size, _, _, _| {
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
            ..
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
        Daemon::new(|| async { vec![] }, |_, _, _| Ok(dummy_pty())).with_router(|_, _, _, _, _| {
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
            ..
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
    // The background quota probe may re-broadcast a pending autonomous
    // task's NoCapacity recommendation before the SessionList reply lands;
    // that duplicate is benign here, only the final session state matters.
    // recv() already bounds each read at 30s, so this loop terminates.
    let session_list = loop {
        match recv(&mut client).await {
            DaemonMessage::RouteRecommendation { .. } => continue,
            other => break other,
        }
    };
    match session_list {
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

    let daemon = Daemon::new(|| async { vec![] }, |_, _, _| Ok(dummy_pty()));

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
            last_seen_offset: None,
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
            model: None,
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
    let daemon = Daemon::new(|| async { vec![] }, |_, _, _| Ok(dummy_pty()));
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

    let daemon_aimem = Daemon::new(|| async { vec![] }, |_, _, _| Ok(dummy_pty()))
        .with_memory_extractor(|_, _, _| async {
            Ok(aihub_memory::HandoffTurn {
                summary: "summary".into(),
                last_output: "output".into(),
                decisions: vec![],
            })
        })
        .with_memory_recorder(|_, _, _, _, _| async {
            Ok(aihub_memory::HandoffDestination::Delivered)
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
    assert_eq!(dest, Some(aihub_memory::HandoffDestination::Delivered));

    let daemon_spool = Daemon::new(|| async { vec![] }, |_, _, _| Ok(dummy_pty()))
        .with_memory_extractor(|_, _, _| async {
            Ok(aihub_memory::HandoffTurn {
                summary: "summary".into(),
                last_output: "output".into(),
                decisions: vec![],
            })
        })
        .with_memory_recorder(|_, _, _, _, _| async {
            Ok(aihub_memory::HandoffDestination::Spooled)
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
    assert_eq!(dest2, Some(aihub_memory::HandoffDestination::Spooled));

    let _ = std::fs::remove_dir_all(&temp_wt);
}

fn exited_pty() -> Pty {
    let (_tx, rx) = broadcast::channel(16);
    Pty {
        output: rx,
        scrollback: vec![],
        write: Box::new(|_| Box::pin(async { Ok(()) })),
        resize: Box::new(|_| Ok(())),
        wait: Box::new(|| Box::pin(async { Ok(Some(0)) })),
        kill: Box::new(|| Box::pin(async { Ok(()) })),
        stop: Arc::new(|_| Box::pin(async { Ok(Some(0)) })),
        try_write: Arc::new(|_| Ok(())),
    }
}

// ---------------------------------------------------------------------------
// Blocker 1: an unconfirmed stop refuses to finalize Git or launch a
// replacement, and leaves the session visibly stopped-with-error.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn f4_merge_refuses_when_stop_unconfirmed() {
    let path = test_socket();
    let finish_calls = Arc::new(AtomicUsize::new(0));
    let finish_count = finish_calls.clone();

    let daemon = Daemon::new(
        || async { vec![] },
        |_, _, _| Ok(failing_stop_for_n_attempts(1).0),
    )
    .with_git_seams(
        |_, _, _| async { Ok("diff content".to_string()) },
        move |_, strategy, _| {
            let finish_count = finish_count.clone();
            async move {
                finish_count.fetch_add(1, Ordering::SeqCst);
                Ok(aihub_git::MergeOutcome {
                    strategy,
                    success: true,
                    diff: "diff content".into(),
                    message: "ok".into(),
                })
            }
        },
    );

    let id = SessionId::new("f4-unconfirmed-session");
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

    // First MergeRequest: preview only.
    send(
        &mut client,
        ClientMessage::MergeRequest {
            session_id: id.clone(),
            strategy: MergeStrategy::Squash,
        },
    )
    .await;
    let _ = recv(&mut client).await;

    // Second MergeRequest: confirmation triggers quiesce, whose stop barrier fails.
    send(
        &mut client,
        ClientMessage::MergeRequest {
            session_id: id.clone(),
            strategy: MergeStrategy::Squash,
        },
    )
    .await;
    match recv(&mut client).await {
        DaemonMessage::Error { code, message } => {
            assert_eq!(code, "stop_unconfirmed");
            assert!(!message.is_empty());
        }
        other => panic!("expected stop_unconfirmed Error, got {other:?}"),
    }
    assert_eq!(
        finish_calls.load(Ordering::SeqCst),
        0,
        "git finalize must never run after an unconfirmed stop"
    );

    // Session stays visible, in a stopped state.
    send(&mut client, ClientMessage::ListSessions).await;
    match recv(&mut client).await {
        DaemonMessage::SessionList { sessions } => {
            assert_eq!(sessions.len(), 1);
            assert!(!sessions[0].active, "session must be visibly stopped");
        }
        other => panic!("expected SessionList, got {other:?}"),
    }

    stop_tx.send(()).unwrap();
    daemon_task.await.unwrap().unwrap();
    if let Some(parent) = path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[tokio::test]
async fn f5_switch_refuses_replacement_when_stop_unconfirmed() {
    let path = test_socket();
    let spawn_calls = Arc::new(AtomicUsize::new(0));
    let calls = spawn_calls.clone();

    let daemon = Daemon::new(
        || async { vec![] },
        move |_, _, _| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(failing_stop_for_n_attempts(1).0)
        },
    );

    let id = SessionId::new("f5-unconfirmed-session");
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
            model: None,
        },
    )
    .await;
    match recv(&mut client).await {
        DaemonMessage::Error { code, .. } => assert_eq!(code, "stop_unconfirmed"),
        other => panic!("expected stop_unconfirmed Error, got {other:?}"),
    }
    assert_eq!(
        spawn_calls.load(Ordering::SeqCst),
        1,
        "the replacement harness must never be launched after an unconfirmed stop"
    );

    send(&mut client, ClientMessage::ListSessions).await;
    match recv(&mut client).await {
        DaemonMessage::SessionList { sessions } => {
            assert_eq!(sessions[0].harness, HarnessId::Codex);
            assert!(!sessions[0].active, "session must be visibly stopped");
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
// F4: a same-group descendant that redirects its stdio and ignores TERM/HUP
// must be fully quiesced (via SIGKILL escalation) before merge or switch
// return, and stays quiet afterward.
// ---------------------------------------------------------------------------

fn write_leader_exits_descendant_script(dir: &std::path::Path) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let script = dir.join("leader-exits.sh");
    std::fs::write(
        &script,
        r#"#!/bin/sh
mkdir -p .scratch
marker=.scratch/marker
: > "$marker"
(trap '' TERM HUP; exec >/dev/null 2>&1 </dev/null; while :; do printf x >> "$marker"; sleep 0.02; done) &
while [ ! -s "$marker" ]; do sleep 0.01; done
exit 0
"#,
    )
    .unwrap();
    std::fs::set_permissions(
        &script,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
    )
    .unwrap();
    script
}

fn write_descendant_script(dir: &std::path::Path) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let script = dir.join("descendant.sh");
    std::fs::write(
        &script,
        r#"#!/bin/sh
mkdir -p .scratch
marker=.scratch/marker
: > "$marker"
(trap '' TERM HUP; exec >/dev/null 2>&1 </dev/null; while :; do printf x >> "$marker"; sleep 0.02; done) &
wait
"#,
    )
    .unwrap();
    std::fs::set_permissions(
        &script,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
    )
    .unwrap();
    script
}

async fn assert_marker_quiet(dir: &std::path::Path) {
    let marker = dir.join(".scratch/marker");
    let deadline = Duration::from_secs(5);
    let stable_for = Duration::from_millis(300);
    tokio::time::timeout(deadline, async {
        let mut last = std::fs::read(&marker).unwrap();
        let mut stable_since = std::time::Instant::now();
        loop {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let current = std::fs::read(&marker).unwrap();
            if current == last {
                if stable_since.elapsed() >= stable_for {
                    return;
                }
            } else {
                last = current;
                stable_since = std::time::Instant::now();
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("marker file never stabilized for {:?}", stable_for));
}

/// Only `HarnessId::Codex` runs the ignoring-descendant script; any other harness (e.g. a
/// switch's incoming target) gets a quiet dummy PTY so it can't itself grow the marker file.
fn descendant_daemon(script: PathBuf) -> Daemon {
    Daemon::new(
        || async { vec![] },
        move |harness, opts, _model| {
            if harness != HarnessId::Codex {
                return Ok(dummy_pty());
            }
            let h = Arc::new(aihub_pty::spawn_command(
                "/bin/sh",
                &[script.to_str().unwrap()],
                opts,
            )?);
            let writer = h.clone();
            let resize = h.clone();
            let wait = h.clone();
            let kill = h.clone();
            let stop = h.clone();
            Ok(Pty {
                output: h.subscribe_output(),
                scrollback: h.scrollback_snapshot(),
                write: Box::new(move |data| {
                    let h = writer.clone();
                    Box::pin(async move { h.try_write(&data).map_err(Into::into) })
                }),
                resize: Box::new(move |size| resize.resize(size).map_err(Into::into)),
                wait: Box::new(move || {
                    let h = wait.clone();
                    Box::pin(async move { h.wait().await.map_err(Into::into) })
                }),
                kill: Box::new(move || {
                    let h = kill.clone();
                    Box::pin(async move { h.kill().await.map_err(Into::into) })
                }),
                stop: Arc::new(move |timeout| {
                    let h = stop.clone();
                    Box::pin(async move { h.stop_barrier(timeout).await.map_err(Into::into) })
                }),
                try_write: Arc::new(move |data| h.try_write(&data).map_err(Into::into)),
            })
        },
    )
}

async fn wait_for_marker(dir: &std::path::Path) {
    let marker = dir.join(".scratch/marker");
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if std::fs::metadata(&marker).is_ok_and(|m| m.len() > 0) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("marker {} never grew", marker.display()));
}

async fn wait_session_stopped(client: &mut UnixStream, id: &SessionId) {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            send(client, ClientMessage::ListSessions).await;
            match recv(client).await {
                DaemonMessage::SessionList { sessions } => {
                    if sessions.iter().any(|s| s.session_id == *id && !s.active) {
                        return;
                    }
                }
                DaemonMessage::SessionExited { session_id, .. } if session_id == *id => return,
                _ => {}
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("session {id} never became inactive within 15s"));
}

#[tokio::test]
async fn f4_redirected_descendant_quiet_after_merge() {
    let root = std::env::temp_dir().join(format!(
        "ah08-f4-merge-{}-{}",
        std::process::id(),
        SOCKET_COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    let script = write_descendant_script(&root);
    let wt = root.join("wt");
    std::fs::create_dir_all(&wt).unwrap();

    let daemon = descendant_daemon(script).with_git_seams(
        |_, _, _| async { Ok(String::new()) },
        |_, strategy, _| async move {
            Ok(aihub_git::MergeOutcome {
                strategy,
                success: true,
                diff: String::new(),
                message: "ok".into(),
            })
        },
    );

    let path = test_socket();
    let id = SessionId::new("f4-descendant-merge");
    daemon
        .add_session(
            PathBuf::from("/fake/repo"),
            aihub_git::SessionWorktree {
                session_id: id.clone(),
                path: wt.clone(),
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
    wait_for_marker(&wt).await;

    send(
        &mut client,
        ClientMessage::MergeRequest {
            session_id: id.clone(),
            strategy: MergeStrategy::Keep,
        },
    )
    .await;
    let _ = recv(&mut client).await; // preview
    send(
        &mut client,
        ClientMessage::MergeRequest {
            session_id: id.clone(),
            strategy: MergeStrategy::Keep,
        },
    )
    .await;
    match recv(&mut client).await {
        DaemonMessage::MergeResult { success, .. } => assert!(success),
        other => panic!("expected MergeResult, got {other:?}"),
    }
    assert_marker_quiet(&wt).await;

    stop_tx.send(()).unwrap();
    daemon_task.await.unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&root);
    if let Some(parent) = path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[tokio::test]
async fn f4_redirected_descendant_quiet_after_switch() {
    let root = std::env::temp_dir().join(format!(
        "ah08-f4-switch-{}-{}",
        std::process::id(),
        SOCKET_COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    let script = write_descendant_script(&root);
    let wt = root.join("wt");
    std::fs::create_dir_all(&wt).unwrap();

    let daemon = descendant_daemon(script);

    let path = test_socket();
    let id = SessionId::new("f4-descendant-switch");
    daemon
        .add_session(
            PathBuf::from("/fake/repo"),
            aihub_git::SessionWorktree {
                session_id: id.clone(),
                path: wt.clone(),
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
    wait_for_marker(&wt).await;

    send(
        &mut client,
        ClientMessage::SwitchHarness {
            session_id: id.clone(),
            target: HarnessId::ClaudeCode,
            with_handoff: false,
            model: None,
        },
    )
    .await;
    match recv(&mut client).await {
        DaemonMessage::HarnessSwitched { .. } => {}
        other => panic!("expected HarnessSwitched, got {other:?}"),
    }
    assert_marker_quiet(&wt).await;

    stop_tx.send(()).unwrap();
    daemon_task.await.unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&root);
    if let Some(parent) = path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[tokio::test]
async fn r1_leader_exited_descendant_quiet_after_merge() {
    let root = std::env::temp_dir().join(format!(
        "ah09-r1-merge-{}-{}",
        std::process::id(),
        SOCKET_COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    let script = write_leader_exits_descendant_script(&root);
    let wt = root.join("wt");
    std::fs::create_dir_all(&wt).unwrap();

    let finish_calls = Arc::new(AtomicUsize::new(0));
    let finish_count = finish_calls.clone();
    let daemon = descendant_daemon(script).with_git_seams(
        |_, _, _| async { Ok(String::new()) },
        move |_, strategy, _| {
            let finish_count = finish_count.clone();
            async move {
                finish_count.fetch_add(1, Ordering::SeqCst);
                Ok(aihub_git::MergeOutcome {
                    strategy,
                    success: true,
                    diff: String::new(),
                    message: "ok".into(),
                })
            }
        },
    );

    let path = test_socket();
    let id = SessionId::new("r1-leader-exit-merge");
    daemon
        .add_session(
            PathBuf::from("/fake/repo"),
            aihub_git::SessionWorktree {
                session_id: id.clone(),
                path: wt.clone(),
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
    wait_for_marker(&wt).await;
    wait_session_stopped(&mut client, &id).await;

    send(
        &mut client,
        ClientMessage::MergeRequest {
            session_id: id.clone(),
            strategy: MergeStrategy::Keep,
        },
    )
    .await;
    match recv_operation_reply(&mut client).await {
        DaemonMessage::MergeResult { .. } => {}
        other => panic!("expected MergeResult preview, got {other:?}"),
    }
    send(
        &mut client,
        ClientMessage::MergeRequest {
            session_id: id.clone(),
            strategy: MergeStrategy::Keep,
        },
    )
    .await;
    match recv_operation_reply(&mut client).await {
        DaemonMessage::MergeResult {
            success, message, ..
        } => assert!(success, "{message}"),
        other => panic!("expected MergeResult, got {other:?}"),
    }
    assert_eq!(
        finish_calls.load(Ordering::SeqCst),
        1,
        "git finalize must run only after the group barrier confirms quiescence"
    );
    assert_marker_quiet(&wt).await;

    stop_tx.send(()).unwrap();
    daemon_task.await.unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&root);
    if let Some(parent) = path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[tokio::test]
async fn r1_leader_exited_descendant_quiet_after_switch() {
    let root = std::env::temp_dir().join(format!(
        "ah09-r1-switch-{}-{}",
        std::process::id(),
        SOCKET_COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    let script = write_leader_exits_descendant_script(&root);
    let wt = root.join("wt");
    std::fs::create_dir_all(&wt).unwrap();

    let spawn_calls = Arc::new(AtomicUsize::new(0));
    let calls = spawn_calls.clone();
    let script_for_spawn = script.clone();
    let daemon = Daemon::new(
        || async { vec![] },
        move |harness, opts, _model| {
            calls.fetch_add(1, Ordering::SeqCst);
            if harness != HarnessId::Codex {
                return Ok(dummy_pty());
            }
            let h = Arc::new(aihub_pty::spawn_command(
                "/bin/sh",
                &[script_for_spawn.to_str().unwrap()],
                opts,
            )?);
            let writer = h.clone();
            let resize = h.clone();
            let wait = h.clone();
            let kill = h.clone();
            let stop = h.clone();
            Ok(Pty {
                output: h.subscribe_output(),
                scrollback: h.scrollback_snapshot(),
                write: Box::new(move |data| {
                    let h = writer.clone();
                    Box::pin(async move { h.try_write(&data).map_err(Into::into) })
                }),
                resize: Box::new(move |size| resize.resize(size).map_err(Into::into)),
                wait: Box::new(move || {
                    let h = wait.clone();
                    Box::pin(async move { h.wait().await.map_err(Into::into) })
                }),
                kill: Box::new(move || {
                    let h = kill.clone();
                    Box::pin(async move { h.kill().await.map_err(Into::into) })
                }),
                stop: Arc::new(move |timeout| {
                    let h = stop.clone();
                    Box::pin(async move { h.stop_barrier(timeout).await.map_err(Into::into) })
                }),
                try_write: Arc::new(move |data| h.try_write(&data).map_err(Into::into)),
            })
        },
    );

    let path = test_socket();
    let id = SessionId::new("r1-leader-exit-switch");
    daemon
        .add_session(
            PathBuf::from("/fake/repo"),
            aihub_git::SessionWorktree {
                session_id: id.clone(),
                path: wt.clone(),
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
    wait_for_marker(&wt).await;
    wait_session_stopped(&mut client, &id).await;

    send(
        &mut client,
        ClientMessage::SwitchHarness {
            session_id: id.clone(),
            target: HarnessId::ClaudeCode,
            with_handoff: false,
            model: None,
        },
    )
    .await;
    match recv_operation_reply(&mut client).await {
        DaemonMessage::HarnessSwitched { .. } => {}
        other => panic!("expected HarnessSwitched, got {other:?}"),
    }
    assert_eq!(
        spawn_calls.load(Ordering::SeqCst),
        2,
        "incoming harness must spawn only after the group barrier confirms quiescence"
    );
    assert_marker_quiet(&wt).await;

    stop_tx.send(()).unwrap();
    daemon_task.await.unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&root);
    if let Some(parent) = path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[tokio::test]
async fn r1_repeated_merge_after_stop_unconfirmed_never_finishes_git() {
    let path = test_socket();
    let finish_calls = Arc::new(AtomicUsize::new(0));
    let finish_count = finish_calls.clone();
    let stop_attempts = Arc::new(AtomicUsize::new(0));
    let attempts = stop_attempts.clone();
    let daemon = Daemon::new(
        || async { vec![] },
        move |_, _, _| {
            let attempts = attempts.clone();
            let mut pty = dummy_pty();
            pty.stop = Arc::new(move |_| {
                let attempts = attempts.clone();
                Box::pin(async move {
                    if attempts.fetch_add(1, Ordering::SeqCst) < 2 {
                        anyhow::bail!("stop barrier signal failed")
                    } else {
                        Ok(Some(0))
                    }
                })
            });
            Ok(pty)
        },
    )
    .with_git_seams(
        |_, _, _| async { Ok("diff".to_string()) },
        move |_, strategy, _| {
            let finish_count = finish_count.clone();
            async move {
                finish_count.fetch_add(1, Ordering::SeqCst);
                Ok(aihub_git::MergeOutcome {
                    strategy,
                    success: true,
                    diff: "diff".into(),
                    message: "ok".into(),
                })
            }
        },
    );

    let id = SessionId::new("r1-repeat-merge");
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

    for _ in 0..2 {
        send(
            &mut client,
            ClientMessage::MergeRequest {
                session_id: id.clone(),
                strategy: MergeStrategy::Squash,
            },
        )
        .await;
        let _ = recv(&mut client).await;
        send(
            &mut client,
            ClientMessage::MergeRequest {
                session_id: id.clone(),
                strategy: MergeStrategy::Squash,
            },
        )
        .await;
        match recv(&mut client).await {
            DaemonMessage::Error { code, .. } => assert_eq!(code, "stop_unconfirmed"),
            other => panic!("expected stop_unconfirmed, got {other:?}"),
        }
    }
    assert_eq!(finish_calls.load(Ordering::SeqCst), 0);
    assert!(
        stop_attempts.load(Ordering::SeqCst) >= 2,
        "each confirm must retry the stop barrier, not trust inactive alone"
    );

    stop_tx.send(()).unwrap();
    daemon_task.await.unwrap().unwrap();
    if let Some(parent) = path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[tokio::test]
async fn r1_second_client_switch_after_stop_unconfirmed_still_refuses_spawn() {
    let path = test_socket();
    let spawn_calls = Arc::new(AtomicUsize::new(0));
    let calls = spawn_calls.clone();

    let daemon = Daemon::new(
        || async { vec![] },
        move |_, _, _| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(failing_stop_for_n_attempts(2).0)
        },
    );

    let id = SessionId::new("r1-second-client");
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
    attach_session(&mut client_a, &id).await;
    send(
        &mut client_a,
        ClientMessage::SwitchHarness {
            session_id: id.clone(),
            target: HarnessId::ClaudeCode,
            with_handoff: false,
            model: None,
        },
    )
    .await;
    match recv(&mut client_a).await {
        DaemonMessage::Error { code, .. } => assert_eq!(code, "stop_unconfirmed"),
        other => panic!("expected stop_unconfirmed, got {other:?}"),
    }

    let mut client_b = connect_and_handshake(&path).await;
    attach_session(&mut client_b, &id).await;
    send(
        &mut client_b,
        ClientMessage::SwitchHarness {
            session_id: id.clone(),
            target: HarnessId::ClaudeCode,
            with_handoff: false,
            model: None,
        },
    )
    .await;
    match recv(&mut client_b).await {
        DaemonMessage::Error { code, .. } => assert_eq!(code, "stop_unconfirmed"),
        other => panic!("expected stop_unconfirmed, got {other:?}"),
    }
    assert_eq!(
        spawn_calls.load(Ordering::SeqCst),
        1,
        "replacement harness must not spawn until quiescence is confirmed"
    );

    stop_tx.send(()).unwrap();
    daemon_task.await.unwrap().unwrap();
    if let Some(parent) = path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

// ---------------------------------------------------------------------------
// F5: a same-harness switch on a stopped session (e.g. after a failed spawn)
// relaunches it, instead of returning early on harness equality alone.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn f5_same_harness_switch_relaunches_stopped_session() {
    let path = test_socket();
    let spawn_calls = Arc::new(AtomicUsize::new(0));
    let calls = spawn_calls.clone();

    let daemon = Daemon::new(
        || async { vec![] },
        move |_, _, _| {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                Ok(exited_pty())
            } else {
                Ok(dummy_pty())
            }
        },
    );

    let id = SessionId::new("f5-relaunch-session");
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

    // The immediate-exit PTY may already have been reaped and broadcast before this
    // client attached; poll the registry instead of racing a one-shot event.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            send(&mut client, ClientMessage::ListSessions).await;
            if let DaemonMessage::SessionList { sessions } = recv(&mut client).await {
                if !sessions[0].active {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("session must become inactive after its immediate exit");

    send(
        &mut client,
        ClientMessage::SwitchHarness {
            session_id: id.clone(),
            target: HarnessId::Codex,
            with_handoff: false,
            model: None,
        },
    )
    .await;
    match recv(&mut client).await {
        DaemonMessage::HarnessSwitched {
            old_harness,
            new_harness,
            ..
        } => {
            assert_eq!(old_harness, HarnessId::Codex);
            assert_eq!(new_harness, HarnessId::Codex);
        }
        other => panic!("expected HarnessSwitched, got {other:?}"),
    }
    assert_eq!(
        spawn_calls.load(Ordering::SeqCst),
        2,
        "same-harness switch on a stopped session must relaunch it"
    );

    send(&mut client, ClientMessage::ListSessions).await;
    match recv(&mut client).await {
        DaemonMessage::SessionList { sessions } => assert!(sessions[0].active),
        other => panic!("expected SessionList, got {other:?}"),
    }

    stop_tx.send(()).unwrap();
    daemon_task.await.unwrap().unwrap();
    if let Some(parent) = path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

// ---------------------------------------------------------------------------
// N1: a silent sidecar (accepted, never answers) must not block other
// clients' requests or a graceful shutdown.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn n1_silent_sidecar_does_not_block_other_clients_or_shutdown() {
    let path = test_socket();
    let temp_wt = std::env::temp_dir().join(format!(
        "ah08-n1-wt-{}-{}",
        std::process::id(),
        SOCKET_COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&temp_wt).unwrap();

    let daemon = Daemon::new(|| async { vec![] }, |_, _, _| Ok(dummy_pty()))
        .with_memory_extractor(|_, _, _| async {
            Ok(aihub_memory::HandoffTurn {
                summary: "s".into(),
                last_output: "o".into(),
                decisions: vec![],
            })
        })
        .with_memory_recorder(|_, _, _, _, _| {
            std::future::pending::<anyhow::Result<aihub_memory::HandoffDestination>>()
        });

    let id = SessionId::new("n1-session");
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

    let mut client_a = connect_and_handshake(&path).await;
    attach_session(&mut client_a, &id).await;

    send(
        &mut client_a,
        ClientMessage::SwitchHarness {
            session_id: id.clone(),
            target: HarnessId::ClaudeCode,
            with_handoff: true,
            model: None,
        },
    )
    .await;
    match recv(&mut client_a).await {
        DaemonMessage::HarnessSwitched { .. } => {}
        other => panic!("expected HarnessSwitched, got {other:?}"),
    }

    // Delivery is now stuck forever. A second client must still get a prompt
    // answer, proving the registry lock was released before delivery (N1).
    let mut client_b = connect_and_handshake(&path).await;
    let listed = tokio::time::timeout(Duration::from_secs(2), async {
        send(&mut client_b, ClientMessage::ListSessions).await;
        recv(&mut client_b).await
    })
    .await
    .expect("ListSessions must answer within 2s while handoff delivery is stuck");
    assert!(matches!(listed, DaemonMessage::SessionList { .. }));

    // Shutdown must still complete even though delivery never returns.
    stop_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), daemon_task)
        .await
        .expect("shutdown must complete while a handoff delivery is stuck")
        .unwrap()
        .unwrap();

    let _ = std::fs::remove_dir_all(&temp_wt);
    if let Some(parent) = path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

// ---------------------------------------------------------------------------
// N6: a hanging model-list executable must not stall sessions or shutdown.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn n6_hanging_model_list_does_not_block_sessions() {
    let dir = std::env::temp_dir().join(format!(
        "ah08-n6-{}-{}",
        std::process::id(),
        SOCKET_COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("cursor-agent");
    std::fs::write(&script, "#!/bin/sh\nexec /bin/sleep 86400\n").unwrap();
    std::fs::set_permissions(
        &script,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
    )
    .unwrap();

    let path = test_socket();
    let daemon =
        Daemon::new(|| async { vec![] }, |_, _, _| Ok(dummy_pty())).with_catalog_paths(move |h| {
            if h == HarnessId::CursorAgent {
                Some(script.clone())
            } else {
                None
            }
        });

    let id = SessionId::new("n6-session");
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

    // Kick off an immediate refresh: this starts the (hanging) catalog
    // discovery on the same telemetry tick, entirely outside the registry lock.
    send(&mut client, ClientMessage::RequestQuota).await;

    let listed = tokio::time::timeout(Duration::from_secs(2), async {
        send(&mut client, ClientMessage::ListSessions).await;
        recv(&mut client).await
    })
    .await
    .expect("ListSessions must answer within 2s while a model-list command hangs");
    assert!(matches!(listed, DaemonMessage::SessionList { .. }));

    // Shutdown must not wait for the hanging child; abort drops it (kill_on_drop).
    stop_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), daemon_task)
        .await
        .expect("shutdown must complete while a model-list command hangs")
        .unwrap()
        .unwrap();

    let _ = std::fs::remove_dir_all(&dir);
    if let Some(parent) = path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

// ---------------------------------------------------------------------------
// F7: a real child that never reads stdin must not block other sessions or
// shutdown (strengthens the injected-error version above).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn f7_real_stalled_writer_isolated_in_daemon() {
    let path = test_socket();

    let daemon = Daemon::new(
        || async { vec![] },
        |harness, opts, _model| {
            if harness == HarnessId::Codex {
                let h = Arc::new(aihub_pty::spawn_command("/bin/sleep", &["86400"], opts)?);
                let writer = h.clone();
                let resize = h.clone();
                let wait = h.clone();
                let kill = h.clone();
                let stop = h.clone();
                Ok(Pty {
                    output: h.subscribe_output(),
                    scrollback: h.scrollback_snapshot(),
                    write: Box::new(move |data| {
                        let h = writer.clone();
                        Box::pin(async move { h.try_write(&data).map_err(Into::into) })
                    }),
                    resize: Box::new(move |size| resize.resize(size).map_err(Into::into)),
                    wait: Box::new(move || {
                        let h = wait.clone();
                        Box::pin(async move { h.wait().await.map_err(Into::into) })
                    }),
                    kill: Box::new(move || {
                        let h = kill.clone();
                        Box::pin(async move { h.kill().await.map_err(Into::into) })
                    }),
                    stop: Arc::new(move |timeout| {
                        let h = stop.clone();
                        Box::pin(async move { h.stop_barrier(timeout).await.map_err(Into::into) })
                    }),
                    try_write: Arc::new(move |data| h.try_write(&data).map_err(Into::into)),
                })
            } else {
                Ok(dummy_pty())
            }
        },
    );

    let id_a = SessionId::new("f7-stalled-session");
    let id_b = SessionId::new("f7-normal-session");
    daemon
        .add_session(
            PathBuf::from("/fake/repo-a"),
            aihub_git::SessionWorktree {
                session_id: id_a.clone(),
                path: PathBuf::from("/fake/wt-a"),
                branch: id_a.branch_name(),
                base_branch: "main".into(),
                originating_checkout: PathBuf::from("/fake/repo-a"),
            },
            HarnessId::Codex,
            None,
        )
        .await
        .unwrap();
    daemon
        .add_session(
            PathBuf::from("/fake/repo-b"),
            aihub_git::SessionWorktree {
                session_id: id_b.clone(),
                path: PathBuf::from("/fake/wt-b"),
                branch: id_b.branch_name(),
                base_branch: "main".into(),
                originating_checkout: PathBuf::from("/fake/repo-b"),
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

    let mut client_a = connect_and_handshake(&path).await;
    let mut client_b = connect_and_handshake(&path).await;

    // Flood A's real PTY (a child that never reads stdin) until its writer thread stalls.
    let chunk = vec![b'x'; 8 * 1024];
    for _ in 0..64 {
        send(
            &mut client_a,
            ClientMessage::PtyInput {
                session_id: id_a.clone(),
                data: chunk.clone().into(),
            },
        )
        .await;
    }

    // B must stay responsive throughout, regardless of A's stalled writer thread.
    let listed = tokio::time::timeout(Duration::from_secs(2), async {
        send(&mut client_b, ClientMessage::ListSessions).await;
        recv(&mut client_b).await
    })
    .await
    .expect("session B must answer within 2s while A's writer is stalled");
    assert!(matches!(listed, DaemonMessage::SessionList { sessions } if sessions.len() == 2));

    stop_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), daemon_task)
        .await
        .expect("shutdown must complete even with a real stalled writer")
        .unwrap()
        .unwrap();

    if let Some(parent) = path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

// ---------------------------------------------------------------------------
// Blocker 5: autonomous dispatch waits for a held recommendation instead of
// silently dropping it, using an injected (paused) clock.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn hold_autonomous_dispatch_waits_for_hold() {
    let path = test_socket();
    let spawn_calls = Arc::new(AtomicUsize::new(0));
    let calls = spawn_calls.clone();

    let temp_wt = std::env::temp_dir().join(format!(
        "ah08-hold-wt-{}-{}",
        std::process::id(),
        SOCKET_COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&temp_wt).unwrap();

    let now_epoch = Arc::new(AtomicU64::new(1_000_000));
    let now_epoch_clock = now_epoch.clone();
    let now_epoch_router = now_epoch.clone();

    let daemon = Daemon::new(
        || async { std::future::pending::<Vec<QuotaSnapshot>>().await },
        move |_, _, _| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(dummy_pty())
        },
    )
    .with_clock(move || now_epoch_clock.load(Ordering::SeqCst))
    .with_classifier(|_| async {
        aihub_router::Classification {
            tier: TaskTier::Mechanical,
            confidence: 0.9,
            ambiguous: false,
        }
    })
    .with_router(move |_, _, _, _, _| {
        Ok(RouteOutcome::Recommendation {
            harness: HarnessId::ClaudeCode,
            lane: None,
            model: Some("claude-held".into()),
            holds_until_s: Some(now_epoch_router.load(Ordering::SeqCst) + 30),
        })
    })
    .with_memory_extractor(|_, _, _| async {
        Ok(aihub_memory::HandoffTurn {
            summary: "s".into(),
            last_output: "o".into(),
            decisions: vec![],
        })
    })
    .with_memory_recorder(|_, _, _, _, _| async { Ok(aihub_memory::HandoffDestination::Spooled) });

    let id = SessionId::new("hold-session");
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
        ClientMessage::SetMode {
            session_id: id.clone(),
            mode: Mode::Autonomous,
        },
    )
    .await;
    match recv(&mut client).await {
        DaemonMessage::ModeSet { .. } => {}
        other => panic!("expected ModeSet, got {other:?}"),
    }

    send(
        &mut client,
        ClientMessage::SubmitTask {
            session_id: id.clone(),
            task: "do a mechanical rename".into(),
        },
    )
    .await;
    match recv(&mut client).await {
        DaemonMessage::RouteRecommendation { outcome, .. } => assert!(matches!(
            outcome,
            RouteOutcome::Recommendation {
                holds_until_s: Some(1_000_030),
                ..
            }
        )),
        other => panic!("expected RouteRecommendation, got {other:?}"),
    }

    // A held recommendation must not dispatch immediately.
    tokio::task::yield_now().await;
    assert_eq!(
        spawn_calls.load(Ordering::SeqCst),
        1,
        "a held recommendation must not launch before the hold passes"
    );

    // Advance the injected clock past the hold; autonomous dispatch must then switch.
    now_epoch.fetch_add(31, Ordering::SeqCst);
    tokio::time::advance(Duration::from_secs(31)).await;
    for _ in 0..500 {
        if spawn_calls.load(Ordering::SeqCst) == 2 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        spawn_calls.load(Ordering::SeqCst),
        2,
        "autonomous dispatch must switch once the hold has passed"
    );

    stop_tx.send(()).unwrap();
    daemon_task.await.unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&temp_wt);
    if let Some(parent) = path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

// ---------------------------------------------------------------------------
// R3: router hold deadline, AcceptRecommendation, and spawn (not SwitchHarness)
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn r3_daemon_accept_recommendation_honors_router_hold_deadline() {
    let path = test_socket();
    let spawn_calls = Arc::new(AtomicUsize::new(0));
    let calls = spawn_calls.clone();
    let temp_wt = std::env::temp_dir().join(format!(
        "ah12-r3-wt-{}-{}",
        std::process::id(),
        SOCKET_COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&temp_wt).unwrap();

    let now_epoch = Arc::new(AtomicU64::new(2_000_000));
    let now_clock = now_epoch.clone();
    let now_router = now_epoch.clone();

    let held_snapshot = vec![QuotaSnapshot {
        slot: SlotId::default_for(HarnessId::Antigravity),
        status: QuotaStatus::Ok,
        source: QuotaSource::Vendor,
        estimated: false,
        note: None,
        windows: vec![],
        lanes: vec![QuotaLane {
            name: "gemini".into(),
            kind: LaneKind::Own,
            windows: vec![QuotaWindow::new(
                WindowKind::FiveHour,
                85.,
                Some(30),
                Some(18_000),
            )],
        }],
    }];
    let catalog = aihub_router::ModelCatalog::from_models(vec![], vec!["gemini-test".into()]);

    let daemon = Daemon::new(
        || async { vec![] },
        move |_, _, _| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(dummy_pty())
        },
    )
    .with_clock(move || now_clock.load(Ordering::SeqCst))
    .with_catalog(catalog)
    .with_classifier(|_| async {
        aihub_router::Classification {
            tier: TaskTier::Mechanical,
            confidence: 0.9,
            ambiguous: false,
        }
    })
    .with_router(move |tier, size, _snapshots, horizon, catalog| {
        aihub_router::route_outcome_at(
            tier,
            size,
            &held_snapshot,
            horizon,
            catalog,
            now_router.load(Ordering::SeqCst),
        )
        .map_err(Into::into)
    })
    .with_memory_extractor(|_, _, _| async {
        Ok(aihub_memory::HandoffTurn {
            summary: "s".into(),
            last_output: "o".into(),
            decisions: vec![],
        })
    })
    .with_memory_recorder(|_, _, _, _, _| async { Ok(aihub_memory::HandoffDestination::Spooled) });

    let id = SessionId::new("r3-hold-session");
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
        ClientMessage::SubmitTask {
            session_id: id.clone(),
            task: "mechanical rename".into(),
        },
    )
    .await;
    let (rec_id, hold_deadline) = match recv(&mut client).await {
        DaemonMessage::RouteRecommendation {
            outcome,
            recommendation_id,
            ..
        } => {
            let deadline = outcome
                .holds_until_s()
                .expect("router hold must be epoch deadline");
            (recommendation_id, deadline)
        }
        other => panic!("expected RouteRecommendation, got {other:?}"),
    };

    send(
        &mut client,
        ClientMessage::AcceptRecommendation {
            session_id: id.clone(),
            recommendation_id: Some(rec_id),
        },
    )
    .await;
    match recv(&mut client).await {
        DaemonMessage::Error { code, .. } => assert_eq!(code, "recommendation_held"),
        other => panic!("expected held error while deadline in future, got {other:?}"),
    }
    assert_eq!(
        spawn_calls.load(Ordering::SeqCst),
        1,
        "accept must not spawn before hold deadline"
    );

    now_epoch.store(hold_deadline, Ordering::SeqCst);
    send(
        &mut client,
        ClientMessage::AcceptRecommendation {
            session_id: id.clone(),
            recommendation_id: Some(rec_id),
        },
    )
    .await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if spawn_calls.load(Ordering::SeqCst) == 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("accept after deadline must switch harness");

    stop_tx.send(()).unwrap();
    daemon_task.await.unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&temp_wt);
    if let Some(parent) = path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

// ---------------------------------------------------------------------------
// R8: same-harness model change reaches spawner argv
// ---------------------------------------------------------------------------

#[tokio::test]
async fn r8_same_harness_model_change_passes_model_to_spawner() {
    let path = test_socket();
    let seen_models = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
    let captured = seen_models.clone();
    let temp_wt = std::env::temp_dir().join(format!(
        "ah12-r8-wt-{}-{}",
        std::process::id(),
        SOCKET_COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&temp_wt).unwrap();

    let daemon = Daemon::new(
        || async { vec![] },
        move |_, _, model| {
            captured.lock().unwrap().push(model);
            Ok(dummy_pty())
        },
    )
    .with_memory_extractor(|_, _, _| async {
        Ok(aihub_memory::HandoffTurn {
            summary: "s".into(),
            last_output: "o".into(),
            decisions: vec![],
        })
    })
    .with_memory_recorder(|_, _, _, _, _| async { Ok(aihub_memory::HandoffDestination::Spooled) });

    let id = SessionId::new("r8-model-session");
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
            HarnessId::CursorAgent,
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
            target: HarnessId::CursorAgent,
            with_handoff: true,
            model: Some("old-model".into()),
        },
    )
    .await;
    let _ = recv(&mut client).await;

    send(
        &mut client,
        ClientMessage::SwitchHarness {
            session_id: id.clone(),
            target: HarnessId::CursorAgent,
            with_handoff: true,
            model: Some("frontier-model".into()),
        },
    )
    .await;
    match recv(&mut client).await {
        DaemonMessage::HarnessSwitched { .. } => {}
        other => panic!("expected HarnessSwitched, got {other:?}"),
    }

    assert_eq!(
        seen_models.lock().unwrap().last().and_then(|m| m.clone()),
        Some("frontier-model".into()),
        "same-harness switch with a new model must reach the spawner"
    );

    stop_tx.send(()).unwrap();
    daemon_task.await.unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&temp_wt);
    if let Some(parent) = path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

// ---------------------------------------------------------------------------
// R10: hold dispatch lifecycle vs quiescence and stale recommendations
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn r10_yielding_extractor_does_not_cancel_hold_dispatch() {
    let path = test_socket();
    let spawn_calls = Arc::new(AtomicUsize::new(0));
    let calls = spawn_calls.clone();
    let extract_gate = Arc::new(tokio::sync::Notify::new());
    let extract_release = extract_gate.clone();
    let temp_wt = std::env::temp_dir().join(format!(
        "ah12-r10-yield-{}-{}",
        std::process::id(),
        SOCKET_COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&temp_wt).unwrap();

    let now_epoch = Arc::new(AtomicU64::new(3_000_000));
    let now_clock = now_epoch.clone();

    let daemon = Daemon::new(
        || async { vec![] },
        move |_, _, _| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(dummy_pty())
        },
    )
    .with_clock(move || now_clock.load(Ordering::SeqCst))
    .with_classifier(|_| async {
        aihub_router::Classification {
            tier: TaskTier::Mechanical,
            confidence: 0.9,
            ambiguous: false,
        }
    })
    .with_router(move |_, _, _, _, _| {
        Ok(RouteOutcome::Recommendation {
            harness: HarnessId::ClaudeCode,
            lane: None,
            model: Some("claude-target".into()),
            holds_until_s: Some(now_epoch.load(Ordering::SeqCst) + 5),
        })
    })
    .with_memory_extractor(move |_, _, _| {
        let gate = extract_gate.clone();
        async move {
            gate.notified().await;
            Ok(aihub_memory::HandoffTurn {
                summary: "s".into(),
                last_output: "o".into(),
                decisions: vec![],
            })
        }
    })
    .with_memory_recorder(|_, _, _, _, _| async { Ok(aihub_memory::HandoffDestination::Spooled) });

    let id = SessionId::new("r10-yield-session");
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
        ClientMessage::SetMode {
            session_id: id.clone(),
            mode: Mode::Autonomous,
        },
    )
    .await;
    let _ = recv(&mut client).await;
    send(
        &mut client,
        ClientMessage::SubmitTask {
            session_id: id.clone(),
            task: "mechanical task".into(),
        },
    )
    .await;
    let _ = recv(&mut client).await;

    tokio::time::advance(Duration::from_secs(6)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        spawn_calls.load(Ordering::SeqCst),
        1,
        "hold timer must start switch without being cancelled by quiesce"
    );

    extract_release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if spawn_calls.load(Ordering::SeqCst) == 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("yielding extractor must not abort the scheduled switch");

    stop_tx.send(()).unwrap();
    daemon_task.await.unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&temp_wt);
    if let Some(parent) = path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[tokio::test(start_paused = true)]
async fn r10_stale_hold_timer_ignored_after_no_capacity() {
    let path = test_socket();
    let spawn_calls = Arc::new(AtomicUsize::new(0));
    let calls = spawn_calls.clone();
    let router_pass = Arc::new(AtomicUsize::new(0));
    let pass = router_pass.clone();
    let temp_wt = std::env::temp_dir().join(format!(
        "ah12-r10-stale-{}-{}",
        std::process::id(),
        SOCKET_COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&temp_wt).unwrap();

    let now_epoch = Arc::new(AtomicU64::new(4_000_000));
    let now_clock = now_epoch.clone();

    let daemon = Daemon::new(
        || async { vec![] },
        move |_, _, _| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(dummy_pty())
        },
    )
    .with_clock(move || now_clock.load(Ordering::SeqCst))
    .with_classifier(|_| async {
        aihub_router::Classification {
            tier: TaskTier::Mechanical,
            confidence: 0.9,
            ambiguous: false,
        }
    })
    .with_router(move |_, _, _, _, _| {
        let n = pass.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            Ok(RouteOutcome::Recommendation {
                harness: HarnessId::ClaudeCode,
                lane: None,
                model: Some("claude-target".into()),
                holds_until_s: Some(now_epoch.load(Ordering::SeqCst) + 60),
            })
        } else {
            Ok(RouteOutcome::NoCapacity {
                reason: "exhausted".into(),
            })
        }
    })
    .with_memory_extractor(|_, _, _| async {
        Ok(aihub_memory::HandoffTurn {
            summary: "s".into(),
            last_output: "o".into(),
            decisions: vec![],
        })
    })
    .with_memory_recorder(|_, _, _, _, _| async { Ok(aihub_memory::HandoffDestination::Spooled) });

    let id = SessionId::new("r10-stale-session");
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
        ClientMessage::SetMode {
            session_id: id.clone(),
            mode: Mode::Autonomous,
        },
    )
    .await;
    let _ = recv(&mut client).await;

    send(
        &mut client,
        ClientMessage::SubmitTask {
            session_id: id.clone(),
            task: "first task".into(),
        },
    )
    .await;
    let _ = recv(&mut client).await;

    send(
        &mut client,
        ClientMessage::SubmitTask {
            session_id: id.clone(),
            task: "second task".into(),
        },
    )
    .await;
    let _ = recv(&mut client).await;

    tokio::time::advance(Duration::from_secs(61)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        spawn_calls.load(Ordering::SeqCst),
        1,
        "stale hold timer must not dispatch after a newer NoCapacity recommendation"
    );

    stop_tx.send(()).unwrap();
    daemon_task.await.unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&temp_wt);
    if let Some(parent) = path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

// ---------------------------------------------------------------------------
// N10: the daemon passes the originating repository identity to the memory
// recorder, not the ephemeral worktree/session directory name.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn n10_switch_passes_repository_identity_to_memory_recorder() {
    let path = test_socket();
    let temp_wt = std::env::temp_dir().join(format!(
        "ah08-n10-sess-uuid-777-abc-{}-{}-{}",
        std::process::id(),
        SOCKET_COUNTER.fetch_add(1, Ordering::SeqCst),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&temp_wt).unwrap();
    let seen_project = Arc::new(Mutex::new(None));
    let captured = seen_project.clone();

    let daemon = Daemon::new(|| async { vec![] }, |_, _, _| Ok(dummy_pty()))
        .with_memory_extractor(|_, _, _| async {
            Ok(aihub_memory::HandoffTurn {
                summary: "s".into(),
                last_output: "o".into(),
                decisions: vec![],
            })
        })
        .with_memory_recorder(move |_, _, _, _, project| {
            *captured.lock().unwrap() = Some(project.to_string());
            async { Ok(aihub_memory::HandoffDestination::Spooled) }
        });

    let id = SessionId::new("sess-uuid-777-abc");
    daemon
        .add_session(
            PathBuf::from("/repos/my-originating-repo"),
            aihub_git::SessionWorktree {
                session_id: id.clone(),
                path: temp_wt.clone(),
                branch: id.branch_name(),
                base_branch: "main".into(),
                originating_checkout: PathBuf::from("/repos/my-originating-repo"),
            },
            HarnessId::Codex,
            None,
        )
        .await
        .unwrap();

    daemon
        .switch_session(&id, HarnessId::ClaudeCode, true)
        .await
        .unwrap();

    let project = seen_project.lock().unwrap().clone();
    assert_eq!(
        project.as_deref(),
        Some("my-originating-repo"),
        "must carry the originating repository identity, not the session/worktree directory name"
    );

    let _ = std::fs::remove_dir_all(&temp_wt);
    if let Some(parent) = path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}
