//! Integration tests for daemon connection, handshake, autostart, and protocol flows.

use aihub_core::{
    encode_frame, ClientMessage, DaemonMessage, IpcMessage, MergeStrategy, SessionId,
    SessionSummary, SessionTarget, PROTOCOL_VERSION,
};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;

use aihub::connection::{perform_handshake, recv_msg, send_msg};
use aihub::terminal::{setup_panic_hook, TerminalGuard};

static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn test_socket_path() -> PathBuf {
    let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    std::env::temp_dir()
        .join(format!(
            "aihub-test-{}-{}-{}",
            std::process::id(),
            count,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
        .join("sock")
}

#[tokio::test]
async fn test_handshake_success() {
    let path = test_socket_path();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let listener = UnixListener::bind(&path).unwrap();

    let server_task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        // Read client Hello
        let len = stream.read_u32().await.unwrap() as usize;
        let mut buf = vec![0u8; len];
        stream.read_exact(&mut buf).await.unwrap();
        let client_msg: IpcMessage = serde_json::from_slice(&buf).unwrap();
        assert_eq!(
            client_msg,
            IpcMessage::Client(ClientMessage::Hello {
                version: PROTOCOL_VERSION,
                credential: None,
            })
        );

        // Send daemon Hello
        let reply = encode_frame(
            &DaemonMessage::Hello {
                version: PROTOCOL_VERSION,
            }
            .into(),
        )
        .unwrap();
        stream.write_all(&reply).await.unwrap();
    });

    let client_stream = tokio::net::UnixStream::connect(&path).await.unwrap();
    let (mut reader, mut writer) = client_stream.into_split();

    perform_handshake(&mut writer, &mut reader).await.unwrap();
    server_task.await.unwrap();

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn test_protocol_send_recv_messages() {
    let path = test_socket_path();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let listener = Arc::new(UnixListener::bind(&path).unwrap());

    let l1 = listener.clone();
    let server_task = tokio::spawn(async move {
        let (stream, _) = l1.accept().await.unwrap();
        let (mut s_reader, _) = stream.into_split();

        // 1. Receive Attach
        let len = s_reader.read_u32().await.unwrap() as usize;
        let mut buf = vec![0u8; len];
        s_reader.read_exact(&mut buf).await.unwrap();
        let client_msg: IpcMessage = serde_json::from_slice(&buf).unwrap();
        assert_eq!(
            client_msg,
            IpcMessage::Client(ClientMessage::Attach {
                target: SessionTarget::Id(SessionId::new("test-session-42")),
                last_seen_offset: None,
            })
        );
    });

    let client_stream = tokio::net::UnixStream::connect(&path).await.unwrap();
    let (_reader, mut writer) = client_stream.into_split();

    // Client sends Attach
    let session = SessionId::new("test-session-42");
    send_msg(
        &mut writer,
        &ClientMessage::Attach {
            target: SessionTarget::Id(session.clone()),
            last_seen_offset: None,
        },
    )
    .await
    .unwrap();

    server_task.await.unwrap();

    // Server sends Attached
    let l2 = listener.clone();
    let s_task = tokio::spawn(async move {
        let (stream, _) = l2.accept().await.unwrap();
        let (_s_reader, mut s_writer) = stream.into_split();
        let attached = DaemonMessage::Attached {
            session_id: SessionId::new("test-session-42"),
            scrollback: b"hello scrollback".to_vec().into(),
            summary: SessionSummary::default(),
            stream_offset: 0,
            gap_detected: false,
            channel_ticket: None,
        };
        send_msg_daemon(&mut s_writer, &attached).await.unwrap();
    });

    let client_stream2 = tokio::net::UnixStream::connect(&path).await.unwrap();
    let (mut reader2, _) = client_stream2.into_split();

    let msg = recv_msg(&mut reader2).await.unwrap();
    assert_eq!(
        msg,
        DaemonMessage::Attached {
            session_id: SessionId::new("test-session-42"),
            scrollback: b"hello scrollback".to_vec().into(),
            summary: SessionSummary::default(),
            stream_offset: 0,
            gap_detected: false,
            channel_ticket: None,
        }
    );

    s_task.await.unwrap();
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

async fn send_msg_daemon(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    msg: &DaemonMessage,
) -> anyhow::Result<()> {
    let frame = encode_frame(&msg.clone().into())?;
    writer.write_all(&frame).await?;
    writer.flush().await?;
    Ok(())
}

#[tokio::test]
async fn test_merge_two_step_confirmation_flow() {
    let path = test_socket_path();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let listener = UnixListener::bind(&path).unwrap();

    let server_task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (mut s_reader, mut s_writer) = stream.into_split();

        // Step 1: Receive first MergeRequest
        let len = s_reader.read_u32().await.unwrap() as usize;
        let mut buf = vec![0u8; len];
        s_reader.read_exact(&mut buf).await.unwrap();
        let msg: IpcMessage = serde_json::from_slice(&buf).unwrap();
        assert_eq!(
            msg,
            IpcMessage::Client(ClientMessage::MergeRequest {
                session_id: SessionId::new("sess-1"),
                strategy: MergeStrategy::Squash,
            })
        );

        // Step 1 response: success = false with diff and review instructions
        let review_result = DaemonMessage::MergeResult {
            session_id: SessionId::new("sess-1"),
            success: false,
            diff: "+ new feature".to_string(),
            message: "Review diff; repeat MergeRequest to confirm.".to_string(),
        };
        send_msg_daemon(&mut s_writer, &review_result)
            .await
            .unwrap();

        // Step 2: Receive confirmed MergeRequest with same strategy
        let len2 = s_reader.read_u32().await.unwrap() as usize;
        let mut buf2 = vec![0u8; len2];
        s_reader.read_exact(&mut buf2).await.unwrap();
        let msg2: IpcMessage = serde_json::from_slice(&buf2).unwrap();
        assert_eq!(
            msg2,
            IpcMessage::Client(ClientMessage::MergeRequest {
                session_id: SessionId::new("sess-1"),
                strategy: MergeStrategy::Squash,
            })
        );

        // Step 2 response: success = true
        let final_result = DaemonMessage::MergeResult {
            session_id: SessionId::new("sess-1"),
            success: true,
            diff: "+ new feature".to_string(),
            message: "Squash merge completed successfully".to_string(),
        };
        send_msg_daemon(&mut s_writer, &final_result).await.unwrap();
    });

    let client_stream = tokio::net::UnixStream::connect(&path).await.unwrap();
    let (mut reader, mut writer) = client_stream.into_split();

    // 1. Client sends initial MergeRequest
    send_msg(
        &mut writer,
        &ClientMessage::MergeRequest {
            session_id: SessionId::new("sess-1"),
            strategy: MergeStrategy::Squash,
        },
    )
    .await
    .unwrap();

    // Client receives review diff
    let review_msg = recv_msg(&mut reader).await.unwrap();
    match review_msg {
        DaemonMessage::MergeResult {
            success,
            diff,
            message,
            ..
        } => {
            assert!(!success);
            assert_eq!(diff, "+ new feature");
            assert!(message.contains("Review diff"));
        }
        other => panic!("Unexpected message: {:?}", other),
    }

    // 2. Client sends confirmation (same strategy)
    send_msg(
        &mut writer,
        &ClientMessage::MergeRequest {
            session_id: SessionId::new("sess-1"),
            strategy: MergeStrategy::Squash,
        },
    )
    .await
    .unwrap();

    // Client receives final confirmation
    let final_msg = recv_msg(&mut reader).await.unwrap();
    match final_msg {
        DaemonMessage::MergeResult {
            success, message, ..
        } => {
            assert!(success);
            assert!(message.contains("completed successfully"));
        }
        other => panic!("Unexpected message: {:?}", other),
    }

    server_task.await.unwrap();
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn test_terminal_hygiene_and_panic_hook() {
    setup_panic_hook();

    // Test TerminalGuard creation and dropping
    // In headless test environments, enable_raw_mode may succeed or fail if stdout is not a TTY.
    // If not a TTY, TerminalGuard::new() returns an error, which is properly handled.
    if let Ok(mut guard) = TerminalGuard::new() {
        guard.restore();
    }
}

#[test]
fn test_cli_argument_parsing() {
    use aihub::cli::{Cli, Commands};
    use clap::Parser;

    // Bare `aihub`
    let cli = Cli::try_parse_from(["aihub"]).unwrap();
    assert!(cli.socket.is_none());
    assert!(cli.command.is_none());

    // `aihub --socket /custom/path`
    let cli = Cli::try_parse_from(["aihub", "--socket", "/custom/path"]).unwrap();
    assert_eq!(cli.socket, Some(PathBuf::from("/custom/path")));
    assert!(cli.command.is_none());

    // `aihub attach`
    let cli = Cli::try_parse_from(["aihub", "attach"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(Commands::Attach { session_id: None })
    ));

    // `aihub attach my-session-id`
    let cli = Cli::try_parse_from(["aihub", "attach", "my-session-id"]).unwrap();
    match cli.command {
        Some(Commands::Attach { session_id }) => {
            assert_eq!(session_id, Some("my-session-id".to_string()));
        }
        _ => panic!("Expected Attach command"),
    }
}

#[tokio::test]
async fn f8_reader_stays_in_sync_when_fragmented_frame_and_key_event_interleave() {
    use aihub::connection::spawn_daemon_reader;

    let path = test_socket_path();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let listener = UnixListener::bind(&path).unwrap();

    let server_task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();

        let msg1 = DaemonMessage::SessionExited {
            session_id: SessionId::new("s1"),
            exit_code: Some(0),
        };
        let msg2 = DaemonMessage::SessionExited {
            session_id: SessionId::new("s2"),
            exit_code: Some(42),
        };

        let frame1 = encode_frame(&msg1.into()).unwrap();
        let frame2 = encode_frame(&msg2.into()).unwrap();

        // Write first half of frame1 (fragmented)
        let split_point = frame1.len() / 2;
        stream.write_all(&frame1[..split_point]).await.unwrap();
        stream.flush().await.unwrap();

        // Wait a bit to allow client to select on a different event (e.g. key/timer)
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;

        // Write second half of frame1 followed immediately by full frame2
        stream.write_all(&frame1[split_point..]).await.unwrap();
        stream.write_all(&frame2).await.unwrap();
        stream.flush().await.unwrap();
    });

    let client_stream = tokio::net::UnixStream::connect(&path).await.unwrap();
    let (reader, _writer) = client_stream.into_split();

    let mut daemon_rx = spawn_daemon_reader(reader);

    // Simulate key event winning select! while frame1 is only partially sent
    let simulated_key_event = tokio::time::sleep(std::time::Duration::from_millis(30));
    tokio::select! {
        _ = simulated_key_event => {
            // Key event handled while daemon frame is in-flight!
        }
        _ = daemon_rx.recv() => {
            panic!("Should not have received message before frame is complete");
        }
    }

    // Now wait for frame1 and frame2 on channel
    let res1 = tokio::time::timeout(std::time::Duration::from_secs(3), daemon_rx.recv())
        .await
        .expect("timeout on msg1")
        .expect("stream ended")
        .expect("msg1 decode error");

    assert_eq!(
        res1,
        DaemonMessage::SessionExited {
            session_id: SessionId::new("s1"),
            exit_code: Some(0),
        },
        "Reader must successfully decode first fragmented message"
    );

    let res2 = tokio::time::timeout(std::time::Duration::from_secs(3), daemon_rx.recv())
        .await
        .expect("timeout on msg2")
        .expect("stream ended")
        .expect("msg2 decode error");

    assert_eq!(
        res2,
        DaemonMessage::SessionExited {
            session_id: SessionId::new("s2"),
            exit_code: Some(42),
        },
        "Reader must stay in sync and decode subsequent message without framing error"
    );

    server_task.await.unwrap();
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn f12_production_initializer_restores_terminal_on_error() {
    use std::sync::atomic::{AtomicBool, Ordering};

    struct FailingWriter;
    impl std::io::Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "simulated terminal write error",
            ))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "simulated terminal flush error",
            ))
        }
    }

    let raw_mode_active = Arc::new(AtomicBool::new(false));
    let raw_clone1 = raw_mode_active.clone();
    let raw_clone2 = raw_mode_active.clone();

    let enable_raw = move || {
        raw_clone1.store(true, Ordering::SeqCst);
        Ok(())
    };

    let disable_raw = move || {
        raw_clone2.store(false, Ordering::SeqCst);
    };

    // Calling the shared initializer (which powers new_with_writer and new) with a failing writer:
    // Raw mode is enabled first, then write/flush fails.
    // The guard must be constructed immediately and restore raw mode before returning error.
    let result = TerminalGuard::new_with_seam(FailingWriter, enable_raw, disable_raw);
    assert!(
        result.is_err(),
        "Initialization with failing writer must error"
    );

    assert!(
        !raw_mode_active.load(Ordering::SeqCst),
        "Raw mode must be disabled after initialization error"
    );
}

#[test]
fn f12_guard_restores_terminal_on_init_error() {
    f12_production_initializer_restores_terminal_on_error();
}

#[test]
fn f13_other_session_events_do_not_leak() {
    use aihub::handle_daemon_msg;
    use aihub::state::App;
    use aihub_core::{HarnessId, Mode, RouteOutcome, SessionSummary};

    let mut app = App::new(PathBuf::from("/test/repo"));
    let my_session = SessionId::new("sess-mine");
    let other_session = SessionId::new("sess-other");

    app.session_id = Some(my_session.clone());
    app.harness = HarnessId::Antigravity;
    app.branch = "session/mine".to_string();
    app.mode = Mode::Autonomous;
    app.active = true;
    app.should_exit = false;

    // 1. Other session's HarnessSwitched must be ignored
    handle_daemon_msg(
        &mut app,
        DaemonMessage::HarnessSwitched {
            session_id: other_session.clone(),
            old_harness: HarnessId::ClaudeCode,
            new_harness: HarnessId::Codex,
            handoff_path: None,
            model: None,
        },
    );
    assert_eq!(
        app.harness,
        HarnessId::Antigravity,
        "Other session harness change must not leak"
    );

    // 2. Other session's ModeSet must be ignored
    handle_daemon_msg(
        &mut app,
        DaemonMessage::ModeSet {
            session_id: other_session.clone(),
            mode: Mode::Assisted,
        },
    );
    assert_eq!(
        app.mode,
        Mode::Autonomous,
        "Other session mode change must not leak"
    );

    // 3. Other session's SessionExited must be ignored
    handle_daemon_msg(
        &mut app,
        DaemonMessage::SessionExited {
            session_id: other_session.clone(),
            exit_code: Some(1),
        },
    );
    assert!(
        app.active,
        "Other session exit must not mark client inactive"
    );

    // 4. Other session's Detached must be ignored
    handle_daemon_msg(
        &mut app,
        DaemonMessage::Detached {
            session_id: other_session.clone(),
        },
    );
    assert!(
        !app.should_exit,
        "Other session detach must not exit client"
    );

    // 5. Other session's PtyOutput must be ignored
    handle_daemon_msg(
        &mut app,
        DaemonMessage::PtyOutput {
            session_id: other_session.clone(),
            data: b"leak content\r\n".to_vec().into(),
            stream_offset: 0,
        },
    );
    assert!(
        !app.vt_parser.screen().contents().contains("leak content"),
        "Other session PTY output must not appear in screen"
    );

    // 6. Other session's RouteRecommendation must be ignored
    handle_daemon_msg(
        &mut app,
        DaemonMessage::RouteRecommendation {
            session_id: other_session.clone(),
            outcome: RouteOutcome::Recommendation {
                harness: HarnessId::Codex,
                lane: None,
                model: None,
                holds_until_s: None,
            },
            recommendation_id: 0,
        },
    );
    assert!(
        app.recommendation.is_none(),
        "Other session recommendation must not leak into client"
    );

    // 7. On Attach, metadata must be taken from summary
    let summary = SessionSummary {
        session_id: my_session.clone(),
        harness: HarnessId::CursorAgent,
        mode: Mode::Assisted,
        repo_path: PathBuf::from("/test/repo"),
        worktree_path: PathBuf::from("/test/repo/wt"),
        branch: "session/feature-x".to_string(),
        active: true,
        model: None,
        lane: None,
    };
    handle_daemon_msg(
        &mut app,
        DaemonMessage::Attached {
            session_id: my_session,
            scrollback: b"clean scrollback".to_vec().into(),
            summary,
            stream_offset: 0,
            gap_detected: false,
            channel_ticket: None,
        },
    );
    assert_eq!(
        app.harness,
        HarnessId::CursorAgent,
        "Harness must be taken from summary"
    );
    assert_eq!(
        app.branch, "session/feature-x",
        "Branch must be taken from summary"
    );
    assert_eq!(app.mode, Mode::Assisted, "Mode must be taken from summary");
    assert!(app.active, "Active status must be taken from summary");
    assert_eq!(app.worktree_path, PathBuf::from("/test/repo/wt"));
}
