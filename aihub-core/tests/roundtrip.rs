use bytes::BytesMut;
use std::path::PathBuf;
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
    codec
        .encode(msg.clone(), &mut bytes)
        .expect("codec.encode failed");
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
            credential: None,
        },
        ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            credential: Some(ClientCredential {
                public_key: Base64Bytes::new(b"pubkey-bytes".to_vec()),
                signature: Base64Bytes::new(b"signature-bytes".to_vec()),
                audience: CREDENTIAL_AUDIENCE.to_string(),
                timestamp: 1_700_000_000,
            }),
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
            last_seen_offset: None,
        },
        ClientMessage::Attach {
            target: SessionTarget::LatestForRepo(repo_path.clone()),
            last_seen_offset: Some(60_000),
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
            model: Some("claude-opus-5".into()),
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
            session_id: sess_id.clone(),
            strategy: MergeStrategy::Discard,
        },
        // 13. SubmitTask (F9)
        ClientMessage::SubmitTask {
            session_id: sess_id.clone(),
            task: "Implement RFC 42 with robust error handling".to_string(),
        },
        // 14. AcceptRecommendation (R3)
        ClientMessage::AcceptRecommendation {
            session_id: sess_id,
            recommendation_id: Some(42),
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
                windows: vec![QuotaWindow::new(
                    WindowKind::FiveHour,
                    20.0,
                    None,
                    Some(18000),
                )],
            },
            QuotaLane {
                name: "third-party".to_string(),
                kind: LaneKind::Frontier,
                windows: vec![QuotaWindow::new(
                    WindowKind::FiveHour,
                    95.0,
                    Some(1800),
                    Some(18000),
                )],
            },
        ],
    };

    let messages = vec![
        // 1. Hello
        DaemonMessage::Hello {
            version: PROTOCOL_VERSION,
        },
        // 1b. Challenge (proof-of-possession, ADR §2.3)
        DaemonMessage::Challenge {
            nonce: Base64Bytes::new(b"nonce-bytes".to_vec()),
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
                model: None,
                lane: None,
            }],
        },
        // 3. SessionCreated
        DaemonMessage::SessionCreated {
            session_id: sess_id.clone(),
            harness: HarnessId::CursorAgent,
            worktree_path: wt_path.clone(),
            branch: "session/session-test-01".to_string(),
            channel_ticket: Some(ChannelTicket(Base64Bytes::new(b"ticket-bytes".to_vec()))),
        },
        // 4. Attached (delta resumption: gap_detected = false)
        DaemonMessage::Attached {
            session_id: sess_id.clone(),
            scrollback: Base64Bytes::new(b"welcome to shell\n$ ".to_vec()),
            summary: SessionSummary {
                session_id: sess_id.clone(),
                harness: HarnessId::ClaudeCode,
                mode: Mode::Assisted,
                repo_path: repo_path.clone(),
                worktree_path: wt_path.clone(),
                branch: "session/session-test-01".to_string(),
                active: true,
                model: None,
                lane: None,
            },
            stream_offset: 100_000,
            gap_detected: false,
            channel_ticket: Some(ChannelTicket(Base64Bytes::new(b"ticket-bytes".to_vec()))),
        },
        // 4b. Attached (buffer overrun: gap_detected = true)
        DaemonMessage::Attached {
            session_id: sess_id.clone(),
            scrollback: Base64Bytes::new(b"full snapshot".to_vec()),
            summary: SessionSummary {
                session_id: sess_id.clone(),
                harness: HarnessId::ClaudeCode,
                mode: Mode::Assisted,
                repo_path: repo_path.clone(),
                worktree_path: wt_path,
                branch: "session/session-test-01".to_string(),
                active: true,
                model: None,
                lane: None,
            },
            stream_offset: 10_485_760,
            gap_detected: true,
            channel_ticket: None,
        },
        // 5. Detached
        DaemonMessage::Detached {
            session_id: sess_id.clone(),
        },
        // 6. PtyOutput
        DaemonMessage::PtyOutput {
            session_id: sess_id.clone(),
            data: Base64Bytes::new(b"\x1b[32mBuild succeeded\x1b[0m\r\n".to_vec()),
            stream_offset: 42,
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
        // 9. RouteRecommendation - dispatchable recommendation (F10, F13)
        DaemonMessage::RouteRecommendation {
            session_id: sess_id.clone(),
            outcome: RouteOutcome::Recommendation {
                harness: HarnessId::ClaudeCode,
                lane: Some("frontier".to_string()),
                model: Some("claude-3-7-sonnet".to_string()),
                holds_until_s: Some(900),
            },
            recommendation_id: 1,
        },
        // 9b. RouteRecommendation - no capacity (F10, F13)
        DaemonMessage::RouteRecommendation {
            session_id: sess_id.clone(),
            outcome: RouteOutcome::NoCapacity {
                reason: "No available slots: every slot is empty or has no supply inside the horizon; do not launch.".to_string(),
            },
            recommendation_id: 2,
        },
        // 10. HarnessSwitched
        DaemonMessage::HarnessSwitched {
            session_id: sess_id.clone(),
            old_harness: HarnessId::ClaudeCode,
            new_harness: HarnessId::Antigravity,
            handoff_path: Some(PathBuf::from("/tmp/aihub/briefs/02.md")),
            model: Some("gemini-2.5-pro".to_string()),
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
        // 14. Unauthorized (ADR §2.3, §6: opaque, no detail)
        DaemonMessage::Unauthorized,
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
        stream_offset: 0,
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
        summary: SessionSummary::default(),
        stream_offset: chunk_size as u64,
        gap_detected: true,
        channel_ticket: None,
    });
    test_roundtrip(attached_msg);
}

#[test]
fn test_pty_binary_frame_roundtrip() {
    // ADR §5: the dedicated PTY channel drops Base64/JSON in favor of a raw binary
    // frame `[8-byte offset][4-byte length][bytes]`. Exercised at both small and
    // 1+ MiB sizes to mirror `test_large_pty_chunks_roundtrip` above.
    for (offset, data) in [
        (0u64, b"hello pty".to_vec()),
        (60_001u64, vec![b'x'; 1_572_864]),
        (u64::MAX - 1, Vec::new()),
    ] {
        let frame = PtyBinaryFrame::new(offset, data);
        let encoded = frame.encode();
        let (decoded, consumed) =
            PtyBinaryFrame::decode(&encoded).expect("expected complete frame");
        assert_eq!(consumed, encoded.len());
        assert_eq!(frame, decoded);
    }

    // Partial buffers must report "not enough data yet", never an error or a false frame.
    let frame = PtyBinaryFrame::new(7, b"partial".to_vec());
    let encoded = frame.encode();
    assert!(PtyBinaryFrame::decode(&encoded[..encoded.len() - 1]).is_none());
    assert!(PtyBinaryFrame::decode(&encoded[..4]).is_none());
}

#[test]
fn test_base64_json_encoding_is_not_array_of_numbers() {
    let pty_data = b"Hello, terminal stream!".to_vec();
    let msg = IpcMessage::Daemon(DaemonMessage::PtyOutput {
        session_id: SessionId::new("sess-1"),
        data: Base64Bytes::new(pty_data),
        stream_offset: 0,
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

#[test]
fn test_f9_submit_task_roundtrip_and_aliases() {
    let sess = SessionId::new("sess-f9");
    let msg = ClientMessage::SubmitTask {
        session_id: sess.clone(),
        task: "Fix findings in parallel crates".to_string(),
    };
    test_roundtrip(IpcMessage::Client(msg));

    // Test alias "prompt"
    let json_prompt =
        r#"{"action":"SubmitTask","payload":{"session_id":"sess-f9","prompt":"Via prompt alias"}}"#;
    let decoded: ClientMessage =
        serde_json::from_str(json_prompt).expect("deserialize prompt alias");
    match decoded {
        ClientMessage::SubmitTask { session_id, task } => {
            assert_eq!(session_id, sess);
            assert_eq!(task, "Via prompt alias");
        }
        _ => panic!("Expected SubmitTask"),
    }
}

#[test]
fn test_f10_route_outcome_typed_and_non_launchable_sentinel() {
    // 1. Dispatchable recommendation
    let rec = RouteOutcome::recommendation(
        HarnessId::CursorAgent,
        Some("frontier".to_string()),
        Some("gpt-4o".to_string()),
        Some(300),
    );
    assert!(rec.is_dispatchable());
    assert_eq!(rec.harness(), Some(HarnessId::CursorAgent));
    assert_eq!(rec.lane(), Some("frontier"));
    assert_eq!(rec.model(), Some("gpt-4o"));
    assert_eq!(rec.holds_until_s(), Some(300));
    assert_eq!(rec.reason(), None);

    let json_rec = serde_json::to_string(&rec).expect("serialize rec");
    assert!(json_rec.contains("\"status\":\"recommendation\""));
    let deserialized_rec: RouteOutcome = serde_json::from_str(&json_rec).expect("deserialize rec");
    assert_eq!(rec, deserialized_rec);

    // 2. No capacity outcome - cannot be mistaken for a launch
    let no_cap = RouteOutcome::no_capacity("Exhausted all quota slots");
    assert!(!no_cap.is_dispatchable());
    assert_eq!(no_cap.harness(), None);
    assert_eq!(no_cap.lane(), None);
    assert_eq!(no_cap.model(), None);
    assert_eq!(no_cap.holds_until_s(), None);
    assert_eq!(no_cap.reason(), Some("Exhausted all quota slots"));

    let json_no_cap = serde_json::to_string(&no_cap).expect("serialize no_cap");
    assert!(json_no_cap.contains("\"status\":\"no_capacity\""));
    let deserialized_no_cap: RouteOutcome =
        serde_json::from_str(&json_no_cap).expect("deserialize no_cap");
    assert_eq!(no_cap, deserialized_no_cap);
}

#[test]
fn test_f13_attached_summary_deserializes_with_default() {
    // Frames without "summary" field must deserialize using SessionSummary::default()
    let json_without_summary =
        r#"{"event":"Attached","payload":{"session_id":"s1","scrollback":"dGVzdA=="}}"#;
    let decoded: DaemonMessage =
        serde_json::from_str(json_without_summary).expect("deserialize old Attached");
    match decoded {
        DaemonMessage::Attached {
            session_id,
            scrollback,
            summary,
            stream_offset,
            gap_detected,
            channel_ticket,
        } => {
            assert_eq!(session_id, SessionId::new("s1"));
            assert_eq!(scrollback.as_slice(), b"test");
            assert_eq!(summary.harness, HarnessId::ClaudeCode);
            assert!(!summary.active);
            assert_eq!(stream_offset, 0);
            assert!(!gap_detected);
            assert_eq!(channel_ticket, None);
        }
        _ => panic!("Expected Attached"),
    }
}

#[test]
fn test_hello_v2_without_credential_still_deserializes() {
    // A v2 client's `Hello { version: 2 }` (no `credential` field at all) must still
    // deserialize under protocol v3 (ADR §2.2 "aceitação incondicional de clientes v2").
    let json_v2_hello = r#"{"action":"Hello","payload":{"version":2}}"#;
    let decoded: ClientMessage =
        serde_json::from_str(json_v2_hello).expect("v2 Hello must still deserialize");
    match decoded {
        ClientMessage::Hello {
            version,
            credential,
        } => {
            assert_eq!(version, 2);
            assert_eq!(credential, None);
        }
        _ => panic!("Expected Hello"),
    }
}

#[test]
fn test_hello_v3_with_credential_roundtrip() {
    let credential = ClientCredential {
        public_key: Base64Bytes::new(b"pubkey".to_vec()),
        signature: Base64Bytes::new(b"sig-over-challenge".to_vec()),
        audience: CREDENTIAL_AUDIENCE.to_string(),
        timestamp: 1_700_000_042,
    };
    let msg = ClientMessage::Hello {
        version: PROTOCOL_VERSION,
        credential: Some(credential.clone()),
    };
    test_roundtrip(IpcMessage::Client(msg));

    let json = serde_json::to_string(&ClientMessage::Hello {
        version: PROTOCOL_VERSION,
        credential: Some(credential),
    })
    .unwrap();
    assert!(json.contains(&format!("\"audience\":\"{CREDENTIAL_AUDIENCE}\"")));
}

#[test]
fn test_attach_resumption_and_gap_detected_roundtrip() {
    let sess_id = SessionId::new("session-resume");

    // Delta catch-up: last_seen_offset within the retained window.
    let attach = ClientMessage::Attach {
        target: SessionTarget::Id(sess_id.clone()),
        last_seen_offset: Some(60_000),
    };
    test_roundtrip(IpcMessage::Client(attach));

    let attached_delta = DaemonMessage::Attached {
        session_id: sess_id.clone(),
        scrollback: Base64Bytes::new(b"[60001..=100000]".to_vec()),
        summary: SessionSummary::default(),
        stream_offset: 100_000,
        gap_detected: false,
        channel_ticket: None,
    };
    test_roundtrip(IpcMessage::Daemon(attached_delta));

    // Buffer overrun: last_seen_offset fell out of the retained window.
    let attached_gap = DaemonMessage::Attached {
        session_id: sess_id,
        scrollback: Base64Bytes::new(b"full 2MiB snapshot".to_vec()),
        summary: SessionSummary::default(),
        stream_offset: 8_388_608,
        gap_detected: true,
        channel_ticket: None,
    };
    test_roundtrip(IpcMessage::Daemon(attached_gap));
}

#[test]
fn test_credential_challenge_channel_ticket_and_pty_channel_hello_roundtrip() {
    let challenge = DaemonMessage::Challenge {
        nonce: Base64Bytes::new(b"one-time-nonce".to_vec()),
    };
    test_roundtrip(IpcMessage::Daemon(challenge));

    let unauthorized = DaemonMessage::Unauthorized;
    test_roundtrip(IpcMessage::Daemon(unauthorized));

    // PtyChannelHello is not itself a ClientMessage/DaemonMessage (the PTY channel
    // stops speaking JSON after this handshake), so it is round-tripped through plain
    // serde_json rather than the IPC frame helpers.
    let hello = PtyChannelHello {
        session_id: SessionId::new("session-pty"),
        ticket: ChannelTicket(Base64Bytes::new(b"one-time-ticket".to_vec())),
    };
    let json = serde_json::to_string(&hello).expect("serialize PtyChannelHello");
    let decoded: PtyChannelHello =
        serde_json::from_str(&json).expect("deserialize PtyChannelHello");
    assert_eq!(hello, decoded);
}
