use std::path::PathBuf;
use bytes::BytesMut;
use tokio_util::codec::{Decoder, Encoder};

use aihub_core::*;

fn test_roundtrip(msg: IpcMessage) {
    // 1. Standalone frame roundtrip
    let framed = encode_frame(&msg).expect("encode_frame failed");
    let (decoded, consumed) = decode_frame(&framed)
        .expect("decode_frame failed")
        .expect("expected complete frame");
    assert_eq!(consumed, framed.len());
    assert_eq!(msg, decoded);

    // 2. Tokio IpcCodec roundtrip
    let mut codec = IpcCodec::default();
    let mut bytes = BytesMut::new();
    codec.encode(msg.clone(), &mut bytes).expect("codec.encode failed");
    let decoded_tokio = codec
        .decode(&mut bytes)
        .expect("codec.decode failed")
        .expect("expected complete message from codec");
    assert_eq!(bytes.len(), 0);
    assert_eq!(msg, decoded_tokio);
}

#[test]
fn test_all_client_messages_roundtrip() {
    let sess_id = SessionId::new("session-test-01");
    let repo_path = PathBuf::from("/workspace/ai-hub");

    let messages = vec![
        // 1. Hello
        ClientMessage::Hello {
            version: PROTOCOL_VERSION,
        },
        // 2. ListSessions
        ClientMessage::ListSessions,
        // 3. NewSession
        ClientMessage::NewSession {
            harness: HarnessId::ClaudeCode,
            repo_path: repo_path.clone(),
            initial_prompt: Some("Implement RFC 42".to_string()),
        },
        ClientMessage::NewSession {
            harness: HarnessId::Antigravity,
            repo_path: repo_path.clone(),
            initial_prompt: None,
        },
        // 4. Attach
        ClientMessage::Attach {
            target: SessionTarget::Id(sess_id.clone()),
        },
        ClientMessage::Attach {
            target: SessionTarget::LatestForRepo(repo_path.clone()),
        },
        // 5. Detach
        ClientMessage::Detach {
            session_id: sess_id.clone(),
        },
        // 6. PtyInput
        ClientMessage::PtyInput {
            session_id: sess_id.clone(),
            data: Base64Bytes::new(b"ls -la\n".to_vec()),
        },
        // 7. PtyResize
        ClientMessage::PtyResize {
            session_id: sess_id.clone(),
            cols: 120,
            rows: 40,
        },
        // 8. RequestQuota
        ClientMessage::RequestQuota,
        // 9. RouteRequest
        ClientMessage::RouteRequest {
            prompt: "Refactor error handling".to_string(),
            repo_path: Some(repo_path),
            size_hint: Some(TaskSize::M),
        },
        // 10. SwitchHarness
        ClientMessage::SwitchHarness {
            session_id: sess_id.clone(),
            target: HarnessId::Codex,
            with_handoff: true,
        },
        // 11. SetMode
        ClientMessage::SetMode {
            session_id: sess_id.clone(),
            mode: Mode::Autonomous,
        },
        // 12. MergeRequest
        ClientMessage::MergeRequest {
            session_id: sess_id.clone(),
            strategy: MergeStrategy::Squash,
        },
        ClientMessage::MergeRequest {
            session_id: sess_id,
            strategy: MergeStrategy::Discard,
        },
    ];

    for client_msg in messages {
        test_roundtrip(IpcMessage::Client(client_msg));
    }
}

#[test]
fn test_all_daemon_messages_roundtrip() {
    let sess_id = SessionId::new("session-test-01");
    let repo_path = PathBuf::from("/workspace/ai-hub");
    let wt_path = PathBuf::from("/tmp/aihub/worktrees/session-test-01");

    let sample_snapshot = QuotaSnapshot {
        slot: SlotId::new(HarnessId::Antigravity, "default"),
        status: QuotaStatus::Low,
        source: QuotaSource::Vendor,
        estimated: false,
        note: Some("approaching 5h rolling limit".to_string()),
        windows: vec![
            QuotaWindow::new(WindowKind::FiveHour, 85.5, Some(1200), Some(18000)),
            QuotaWindow::new(WindowKind::SevenDay, 30.0, Some(86400 * 3), Some(604800)),
        ],
        lanes: vec![
            QuotaLane {
                name: "gemini".to_string(),
                kind: LaneKind::Own,
                windows: vec![QuotaWindow::new(WindowKind::FiveHour, 20.0, None, Some(18000))],
            },
            QuotaLane {
                name: "third-party".to_string(),
                kind: LaneKind::Frontier,
                windows: vec![QuotaWindow::new(WindowKind::FiveHour, 95.0, Some(1800), Some(18000))],
            },
        ],
    };

    let messages = vec![
        // 1. Hello
        DaemonMessage::Hello {
            version: PROTOCOL_VERSION,
        },
        // 2. SessionList
        DaemonMessage::SessionList {
            sessions: vec![SessionSummary {
                session_id: sess_id.clone(),
                harness: HarnessId::ClaudeCode,
                mode: Mode::Assisted,
                repo_path: repo_path.clone(),
                worktree_path: wt_path.clone(),
                branch: "session/session-test-01".to_string(),
                active: true,
            }],
        },
        // 3. SessionCreated
        DaemonMessage::SessionCreated {
            session_id: sess_id.clone(),
            harness: HarnessId::CursorAgent,
            worktree_path: wt_path,
            branch: "session/session-test-01".to_string(),
        },
        // 4. Attached
        DaemonMessage::Attached {
            session_id: sess_id.clone(),
            scrollback: Base64Bytes::new(b"welcome to shell\n$ ".to_vec()),
        },
        // 5. Detached
        DaemonMessage::Detached {
            session_id: sess_id.clone(),
        },
        // 6. PtyOutput
        DaemonMessage::PtyOutput {
            session_id: sess_id.clone(),
            data: Base64Bytes::new(b"\x1b[32mBuild succeeded\x1b[0m\r\n".to_vec()),
        },
        // 7. SessionExited
        DaemonMessage::SessionExited {
            session_id: sess_id.clone(),
            exit_code: Some(0),
        },
        // 8. QuotaPush
        DaemonMessage::QuotaPush {
            snapshots: vec![sample_snapshot],
        },
        // 9. RouteRecommendation
        DaemonMessage::RouteRecommendation {
            tier: TaskTier::Design,
            harness: HarnessId::ClaudeCode,
            lane: Some("frontier".to_string()),
            holds_until_s: Some(900),
            confidence: 0.92,
            reason: "Architecture prompt requires frontier reasoning tier".to_string(),
        },
        // 10. HarnessSwitched
        DaemonMessage::HarnessSwitched {
            session_id: sess_id.clone(),
            old_harness: HarnessId::ClaudeCode,
            new_harness: HarnessId::Antigravity,
            handoff_path: Some(PathBuf::from("/tmp/aihub/briefs/02.md")),
        },
        // 11. ModeSet
        DaemonMessage::ModeSet {
            session_id: sess_id.clone(),
            mode: Mode::Autonomous,
        },
        // 12. MergeResult
        DaemonMessage::MergeResult {
            session_id: sess_id.clone(),
            success: true,
            diff: "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new\n".to_string(),
            message: "Squash merge completed into main".to_string(),
        },
        // 13. Error
        DaemonMessage::Error {
            code: "ERR_NO_QUOTA".to_string(),
            message: "All quota slots exhausted".to_string(),
        },
    ];

    for daemon_msg in messages {
        test_roundtrip(IpcMessage::Daemon(daemon_msg));
    }
}

#[test]
fn test_large_pty_chunks_roundtrip() {
    // Test PTY chunks of at least 1 MiB (using 1.5 MiB = 1,572,864 bytes)
    let chunk_size = 1_572_864; // 1.5 MiB
    let mut large_data = Vec::with_capacity(chunk_size);
    for i in 0..chunk_size {
        large_data.push((i % 256) as u8);
    }
    assert_eq!(large_data.len(), chunk_size);

    let sess_id = SessionId::new("session-large-pty");

    // 1. Large PTY Output chunk
    let pty_output_msg = IpcMessage::Daemon(DaemonMessage::PtyOutput {
        session_id: sess_id.clone(),
        data: Base64Bytes::new(large_data.clone()),
    });
    test_roundtrip(pty_output_msg);

    // 2. Large PTY Input chunk
    let pty_input_msg = IpcMessage::Client(ClientMessage::PtyInput {
        session_id: sess_id.clone(),
        data: Base64Bytes::new(large_data.clone()),
    });
    test_roundtrip(pty_input_msg);

    // 3. Large Scrollback replay
    let attached_msg = IpcMessage::Daemon(DaemonMessage::Attached {
        session_id: sess_id,
        scrollback: Base64Bytes::new(large_data),
    });
    test_roundtrip(attached_msg);
}

#[test]
fn test_base64_json_encoding_is_not_array_of_numbers() {
    let pty_data = b"Hello, terminal stream!".to_vec();
    let msg = IpcMessage::Daemon(DaemonMessage::PtyOutput {
        session_id: SessionId::new("sess-1"),
        data: Base64Bytes::new(pty_data),
    });

    let json_str = serde_json::to_string(&msg).expect("serialize json");
    // Verify that the JSON contains standard base64 string and NOT "[72,101,108..."
    assert!(
        json_str.contains("\"SGVsbG8sIHRlcm1pbmFsIHN0cmVhbSE=\""),
        "Expected base64 string in JSON output, got: {json_str}"
    );
    assert!(
        !json_str.contains("[72,101,108"),
        "PTY payload must not be a JSON array of numbers"
    );
}
