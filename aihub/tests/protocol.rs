//! Integration tests for daemon connection, handshake, autostart, and protocol flows.

use std::path::PathBuf;
use std::sync::Arc;
use aihub_core::{
    encode_frame, ClientMessage, DaemonMessage, IpcMessage, MergeStrategy,
    SessionId, SessionTarget, PROTOCOL_VERSION,
};
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
                version: PROTOCOL_VERSION
            })
        );

        // Send daemon Hello
        let reply = encode_frame(&DaemonMessage::Hello {
            version: PROTOCOL_VERSION,
        }.into()).unwrap();
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
        send_msg_daemon(&mut s_writer, &review_result).await.unwrap();

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
        DaemonMessage::MergeResult { success, message, .. } => {
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
    use clap::Parser;
    use aihub::cli::{Cli, Commands};

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
    assert!(matches!(cli.command, Some(Commands::Attach { session_id: None })));

    // `aihub attach my-session-id`
    let cli = Cli::try_parse_from(["aihub", "attach", "my-session-id"]).unwrap();
    match cli.command {
        Some(Commands::Attach { session_id }) => {
            assert_eq!(session_id, Some("my-session-id".to_string()));
        }
        _ => panic!("Expected Attach command"),
    }
}

