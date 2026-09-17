//! Keystroke routing, prefix chord, and palette execution tests.

use aihub_core::{ClientMessage, HarnessId, MergeStrategy, Mode, SessionId, TaskTier};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::path::PathBuf;

use aihub::keys::{cycle_harness, handle_key, key_event_to_bytes, AppAction};
use aihub::state::{App, RecommendationState, UiMode};

fn make_key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, modifiers)
}

fn plain_key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

#[test]
fn test_plain_keystrokes_go_to_pty() {
    let mut app = App::new(PathBuf::from("/test/repo"));
    app.session_id = Some(SessionId::new("test-session"));

    // Direct key_event_to_bytes test
    assert_eq!(
        key_event_to_bytes(plain_key(KeyCode::Char('z'))),
        Some(b"z".to_vec())
    );
    assert_eq!(
        key_event_to_bytes(plain_key(KeyCode::Esc)),
        Some(vec![0x1B])
    );

    // Letter 'a'
    let action = handle_key(&mut app, plain_key(KeyCode::Char('a')));
    assert_eq!(action, AppAction::SendPtyInput(b"a".to_vec()));

    // Enter
    let action = handle_key(&mut app, plain_key(KeyCode::Enter));
    assert_eq!(action, AppAction::SendPtyInput(b"\r".to_vec()));

    // Tab
    let action = handle_key(&mut app, plain_key(KeyCode::Tab));
    assert_eq!(action, AppAction::SendPtyInput(b"\t".to_vec()));

    // Backspace
    let action = handle_key(&mut app, plain_key(KeyCode::Backspace));
    assert_eq!(action, AppAction::SendPtyInput(vec![0x7F]));

    // Up arrow
    let action = handle_key(&mut app, plain_key(KeyCode::Up));
    assert_eq!(action, AppAction::SendPtyInput(b"\x1b[A".to_vec()));

    // Ctrl+C
    let action = handle_key(
        &mut app,
        make_key(KeyCode::Char('c'), KeyModifiers::CONTROL),
    );
    assert_eq!(action, AppAction::SendPtyInput(vec![3]));

    // Ctrl+D
    let action = handle_key(
        &mut app,
        make_key(KeyCode::Char('d'), KeyModifiers::CONTROL),
    );
    assert_eq!(action, AppAction::SendPtyInput(vec![4]));
}

#[test]
fn test_prefix_chord_activation() {
    let mut app = App::new(PathBuf::from("/test/repo"));
    app.session_id = Some(SessionId::new("test-session"));
    assert!(!app.prefix_active);

    // Ctrl+] activates prefix chord
    let ctrl_bracket = make_key(KeyCode::Char(']'), KeyModifiers::CONTROL);
    let action = handle_key(&mut app, ctrl_bracket);
    assert_eq!(
        action,
        AppAction::None,
        "Prefix chord should not be sent to PTY"
    );
    assert!(app.prefix_active, "Prefix chord should be marked active");
}

#[test]
fn test_prefix_actions() {
    let mut app = App::new(PathBuf::from("/test/repo"));
    let session = SessionId::new("test-session");
    app.session_id = Some(session.clone());
    app.harness = HarnessId::ClaudeCode;
    app.mode = Mode::Assisted;
    app.task = Some("existing task".to_string());
    app.recommendation = Some(RecommendationState::Recommended {
        tier: TaskTier::Mechanical,
        harness: HarnessId::Antigravity,
        lane: None,
        model: None,
        holds_until_s: None,
        confidence: 0.9,
        reason: "mechanical task".to_string(),
        recommendation_id: 0,
    });

    let ctrl_bracket = make_key(KeyCode::Char(']'), KeyModifiers::CONTROL);

    // 1. Prefix + 'p' -> open palette
    handle_key(&mut app, ctrl_bracket);
    assert!(app.prefix_active);
    let action = handle_key(&mut app, plain_key(KeyCode::Char('p')));
    assert_eq!(action, AppAction::None);
    assert!(!app.prefix_active);
    assert!(matches!(app.ui_mode, UiMode::Palette { .. }));
    app.ui_mode = UiMode::Normal;

    // 2. Prefix + ':' -> open palette
    handle_key(&mut app, ctrl_bracket);
    let action = handle_key(&mut app, plain_key(KeyCode::Char(':')));
    assert_eq!(action, AppAction::None);
    assert!(matches!(app.ui_mode, UiMode::Palette { .. }));
    app.ui_mode = UiMode::Normal;

    // 3. Prefix + Enter -> accept recommendation
    handle_key(&mut app, ctrl_bracket);
    let action = handle_key(&mut app, plain_key(KeyCode::Enter));
    assert_eq!(
        action,
        AppAction::SendMessage(ClientMessage::AcceptRecommendation {
            session_id: session.clone(),
            recommendation_id: Some(0),
        })
    );

    // 4. Prefix + Tab -> cycle harness
    handle_key(&mut app, ctrl_bracket);
    let action = handle_key(&mut app, plain_key(KeyCode::Tab));
    let expected_next = cycle_harness(HarnessId::ClaudeCode);
    assert_eq!(
        action,
        AppAction::SendMessage(ClientMessage::SwitchHarness {
            session_id: session.clone(),
            target: expected_next,
            with_handoff: true,
            model: None,
        })
    );

    // 5. Prefix + 'm' -> toggle mode
    handle_key(&mut app, ctrl_bracket);
    let action = handle_key(&mut app, plain_key(KeyCode::Char('m')));
    assert_eq!(
        action,
        AppAction::SendMessage(ClientMessage::SetMode {
            session_id: session.clone(),
            mode: Mode::Autonomous,
        })
    );

    // 6. Prefix + 'q' -> quota table
    handle_key(&mut app, ctrl_bracket);
    let action = handle_key(&mut app, plain_key(KeyCode::Char('q')));
    assert_eq!(action, AppAction::None);
    assert_eq!(app.ui_mode, UiMode::QuotaTable { scroll: 0 });
    app.ui_mode = UiMode::Normal;

    // 7. Prefix + 'd' -> detach
    handle_key(&mut app, ctrl_bracket);
    let action = handle_key(&mut app, plain_key(KeyCode::Char('d')));
    assert_eq!(
        action,
        AppAction::SendMessage(ClientMessage::Detach {
            session_id: session.clone(),
        })
    );

    // 8. Prefix + Ctrl+] -> literal Ctrl+] to PTY
    handle_key(&mut app, ctrl_bracket);
    let action = handle_key(&mut app, ctrl_bracket);
    assert_eq!(action, AppAction::SendPtyInput(vec![0x1D]));
    assert!(!app.prefix_active);

    // 9. Prefix + other key -> cancels prefix mode without sending to PTY
    handle_key(&mut app, ctrl_bracket);
    assert!(app.prefix_active);
    let action = handle_key(&mut app, plain_key(KeyCode::Char('x')));
    assert_eq!(action, AppAction::None);
    assert!(!app.prefix_active);
}

#[test]
fn test_palette_commands_execution() {
    let mut app = App::new(PathBuf::from("/test/repo"));
    let session = SessionId::new("test-session");
    app.session_id = Some(session.clone());
    app.task = Some("test task".to_string());

    // Test /switch agy
    let action = aihub::keys::execute_palette_command(&mut app, "/switch agy");
    assert_eq!(
        action,
        AppAction::SendMessage(ClientMessage::SwitchHarness {
            session_id: session.clone(),
            target: HarnessId::Antigravity,
            with_handoff: true,
            model: None,
        })
    );

    // Test /switch claude
    let action = aihub::keys::execute_palette_command(&mut app, "/switch claude");
    assert_eq!(
        action,
        AppAction::SendMessage(ClientMessage::SwitchHarness {
            session_id: session.clone(),
            target: HarnessId::ClaudeCode,
            with_handoff: true,
            model: None,
        })
    );

    // Test /merge
    let action = aihub::keys::execute_palette_command(&mut app, "/merge");
    assert_eq!(
        action,
        AppAction::SendMessage(ClientMessage::MergeRequest {
            session_id: session.clone(),
            strategy: MergeStrategy::Squash,
        })
    );

    // Test /quota
    let action = aihub::keys::execute_palette_command(&mut app, "/quota");
    assert_eq!(action, AppAction::None);
    assert_eq!(app.ui_mode, UiMode::QuotaTable { scroll: 0 });

    // Test /mode
    app.mode = Mode::Assisted;
    let action = aihub::keys::execute_palette_command(&mut app, "/mode");
    assert_eq!(
        action,
        AppAction::SendMessage(ClientMessage::SetMode {
            session_id: session.clone(),
            mode: Mode::Autonomous,
        })
    );

    // Test /detach
    let action = aihub::keys::execute_palette_command(&mut app, "/detach");
    assert_eq!(
        action,
        AppAction::SendMessage(ClientMessage::Detach {
            session_id: session.clone(),
        })
    );
}

#[test]
fn test_merge_review_keys() {
    let mut app = App::new(PathBuf::from("/test/repo"));
    let session = SessionId::new("test-session");
    app.session_id = Some(session.clone());

    app.ui_mode = UiMode::MergeReview {
        diff: "+ added line\n- removed line".to_string(),
        message: "Review diff".to_string(),
        strategy: MergeStrategy::Squash,
        scroll: 0,
    };

    // Selecting Fast-Forward with 'f'
    let action = handle_key(&mut app, plain_key(KeyCode::Char('f')));
    assert_eq!(
        action,
        AppAction::SendMessage(ClientMessage::MergeRequest {
            session_id: session.clone(),
            strategy: MergeStrategy::FastForward,
        })
    );
    match app.ui_mode {
        UiMode::MergeReview { strategy, .. } => assert_eq!(strategy, MergeStrategy::FastForward),
        _ => panic!("Expected MergeReview"),
    }

    // Selecting Keep with 'k'
    let action = handle_key(&mut app, plain_key(KeyCode::Char('k')));
    assert_eq!(
        action,
        AppAction::SendMessage(ClientMessage::MergeRequest {
            session_id: session.clone(),
            strategy: MergeStrategy::Keep,
        })
    );

    // Confirming with Enter sends current strategy
    let action = handle_key(&mut app, plain_key(KeyCode::Enter));
    assert_eq!(
        action,
        AppAction::SendMessage(ClientMessage::MergeRequest {
            session_id: session.clone(),
            strategy: MergeStrategy::Keep,
        })
    );

    // Cancelling with Esc
    let action = handle_key(&mut app, plain_key(KeyCode::Esc));
    assert_eq!(action, AppAction::None);
    assert_eq!(app.ui_mode, UiMode::Normal);
}

#[test]
fn test_cycle_harness() {
    assert_eq!(cycle_harness(HarnessId::ClaudeCode), HarnessId::Antigravity);
    assert_eq!(cycle_harness(HarnessId::Antigravity), HarnessId::Codex);
    assert_eq!(cycle_harness(HarnessId::Codex), HarnessId::CursorAgent);
    assert_eq!(cycle_harness(HarnessId::CursorAgent), HarnessId::ClaudeCode);
}

#[test]
fn f9_task_submission_via_palette() {
    let mut app = App::new(PathBuf::from("/test/repo"));
    let session = SessionId::new("sess-f9");
    app.session_id = Some(session.clone());

    // 1. /task command via palette
    let action = aihub::keys::execute_palette_command(&mut app, "/task improve test coverage");
    assert_eq!(
        action,
        AppAction::SendMessage(ClientMessage::SubmitTask {
            session_id: session.clone(),
            task: "improve test coverage".to_string(),
        })
    );
    assert!(
        app.status_message.is_some(),
        "Status message should acknowledge submitted task"
    );

    // 2. Empty task shows usage and sends nothing
    let action_empty = aihub::keys::execute_palette_command(&mut app, "/task");
    assert_eq!(action_empty, AppAction::None);

    // 3. Normal typing to PTY never generates SubmitTask
    let pty_action = handle_key(&mut app, plain_key(KeyCode::Char('x')));
    assert_eq!(pty_action, AppAction::SendPtyInput(b"x".to_vec()));
}

#[test]
fn f9_task_submission_via_cli_argument() {
    use aihub::cli::Cli;
    use clap::Parser;

    // Bare aihub has no task
    let bare = Cli::try_parse_from(["aihub"]).unwrap();
    assert!(bare.task.is_none());

    // aihub with task argument
    let with_task = Cli::try_parse_from(["aihub", "implement user login"]).unwrap();
    assert_eq!(with_task.task, Some("implement user login".to_string()));
    assert!(with_task.command.is_none());

    // aihub with socket and task argument
    let with_sock_and_task =
        Cli::try_parse_from(["aihub", "--socket", "/tmp/sock", "run benchmarks"]).unwrap();
    assert_eq!(with_sock_and_task.task, Some("run benchmarks".to_string()));
    assert_eq!(with_sock_and_task.socket, Some(PathBuf::from("/tmp/sock")));
}

#[test]
fn f10_accept_on_no_capacity_sends_nothing() {
    let mut app = App::new(PathBuf::from("/test/repo"));
    let session = SessionId::new("sess-f10");
    app.session_id = Some(session.clone());
    app.mode = Mode::Assisted;

    // Set NoCapacity outcome
    app.recommendation = Some(RecommendationState::NoCapacity {
        reason: "All provider quota windows exhausted".to_string(),
    });

    let ctrl_bracket = make_key(KeyCode::Char(']'), KeyModifiers::CONTROL);

    // Activate prefix
    handle_key(&mut app, ctrl_bracket);
    assert!(app.prefix_active);

    // Press Enter to accept
    let action = handle_key(&mut app, plain_key(KeyCode::Enter));

    // Must NOT send SwitchHarness or any client message
    assert_eq!(
        action,
        AppAction::None,
        "Accepting NoCapacity must send nothing"
    );

    // Must inform user in status
    assert!(
        app.status_message.is_some(),
        "Status should explain that acceptance is unavailable"
    );
    let (status_text, _) = app.status_message.unwrap();
    assert!(
        status_text.contains("Não é possível aceitar")
            && status_text.contains("All provider quota windows exhausted"),
        "Status should contain reason: got '{}'",
        status_text
    );
}

#[test]
fn test_tab_cycles_available_harnesses_only() {
    use aihub::keys::cycle_available_harness;
    use aihub_core::{QuotaSnapshot, QuotaStatus, SlotId};

    let snapshots = vec![
        QuotaSnapshot {
            slot: SlotId::new(HarnessId::ClaudeCode, "def"),
            status: QuotaStatus::Ok,
            source: aihub_core::QuotaSource::Vendor,
            estimated: false,
            note: None,
            windows: vec![],
            lanes: vec![],
        },
        QuotaSnapshot {
            slot: SlotId::new(HarnessId::Antigravity, "def"),
            status: QuotaStatus::Unknown, // Not available
            source: aihub_core::QuotaSource::Vendor,
            estimated: false,
            note: None,
            windows: vec![],
            lanes: vec![],
        },
        QuotaSnapshot {
            slot: SlotId::new(HarnessId::Codex, "def"),
            status: QuotaStatus::Empty, // Not available
            source: aihub_core::QuotaSource::Vendor,
            estimated: false,
            note: None,
            windows: vec![],
            lanes: vec![],
        },
        QuotaSnapshot {
            slot: SlotId::new(HarnessId::CursorAgent, "def"),
            status: QuotaStatus::Low, // Available!
            source: aihub_core::QuotaSource::Vendor,
            estimated: false,
            note: None,
            windows: vec![],
            lanes: vec![],
        },
    ];

    // Starting at ClaudeCode, next available MUST skip Antigravity (Unknown) and Codex (Empty) -> CursorAgent
    let next1 = cycle_available_harness(HarnessId::ClaudeCode, &snapshots);
    assert_eq!(next1, HarnessId::CursorAgent);

    // Starting at CursorAgent, next available cycles back to ClaudeCode
    let next2 = cycle_available_harness(HarnessId::CursorAgent, &snapshots);
    assert_eq!(next2, HarnessId::ClaudeCode);

    // Starting at Codex (which is Empty), picking next available jumps to first available
    let next3 = cycle_available_harness(HarnessId::Codex, &snapshots);
    assert!(next3 == HarnessId::ClaudeCode || next3 == HarnessId::CursorAgent);

    // Test in handle_normal_key with Tab
    let mut app = App::new(PathBuf::from("/test/repo"));
    let session = SessionId::new("sess-tab");
    app.session_id = Some(session.clone());
    app.harness = HarnessId::ClaudeCode;
    app.snapshots = snapshots;

    let ctrl_bracket = make_key(KeyCode::Char(']'), KeyModifiers::CONTROL);
    handle_key(&mut app, ctrl_bracket);
    let action = handle_key(&mut app, plain_key(KeyCode::Tab));
    assert_eq!(
        action,
        AppAction::SendMessage(ClientMessage::SwitchHarness {
            session_id: session,
            target: HarnessId::CursorAgent,
            with_handoff: true,
            model: None,
        })
    );
}

#[test]
fn test_merge_strategy_preserved_across_merge_result() {
    use aihub::handle_daemon_msg;
    use aihub_core::DaemonMessage;

    let mut app = App::new(PathBuf::from("/test/repo"));
    let session = SessionId::new("sess-merge");
    app.session_id = Some(session.clone());

    // User previously selected FastForward in merge review
    app.ui_mode = UiMode::MergeReview {
        diff: "+ original diff".to_string(),
        message: "Review diff".to_string(),
        strategy: MergeStrategy::FastForward,
        scroll: 3,
    };

    // Daemon sends initial/updated review diff with success: false
    let msg = DaemonMessage::MergeResult {
        session_id: session.clone(),
        success: false,
        diff: "+ updated diff".to_string(),
        message: "Confirm merge".to_string(),
    };
    handle_daemon_msg(&mut app, msg);

    // Strategy must remain FastForward, NOT reset to Squash
    match &app.ui_mode {
        UiMode::MergeReview {
            strategy,
            diff,
            scroll,
            ..
        } => {
            assert_eq!(
                *strategy,
                MergeStrategy::FastForward,
                "Merge strategy must be preserved"
            );
            assert_eq!(diff, "+ updated diff");
            assert_eq!(*scroll, 3);
        }
        _ => panic!("Expected UiMode::MergeReview"),
    }
}

#[test]
fn f9_autonomous_without_task_opens_task_prompt() {
    let mut app = App::new(PathBuf::from("/test/repo"));
    let session = SessionId::new("sess-f9-notask");
    app.session_id = Some(session.clone());
    app.mode = Mode::Assisted;
    assert!(!app.has_task());

    let ctrl_bracket = make_key(KeyCode::Char(']'), KeyModifiers::CONTROL);

    // Press Ctrl+] then 'm' to toggle Autonomous
    let act1 = handle_key(&mut app, ctrl_bracket);
    assert_eq!(act1, AppAction::None);
    assert!(app.prefix_active);

    let act2 = handle_key(&mut app, plain_key(KeyCode::Char('m')));
    // Must NOT emit SetMode IPC message
    assert_eq!(
        act2,
        AppAction::None,
        "Toggling autonomous without a task must emit no IPC message"
    );

    // Must open task prompt
    match &app.ui_mode {
        UiMode::Palette { input, .. } => {
            assert!(
                input.starts_with("/task"),
                "Palette input must be pre-filled with task prompt, got: '{}'",
                input
            );
        }
        other => panic!("Expected UiMode::Palette, got {:?}", other),
    }

    // Must show a one-line notice explaining that Autonomous needs a task
    assert!(
        app.status_message.is_some(),
        "Notice must be displayed when autonomous toggle is gated by task"
    );
    let (notice, _) = app.status_message.as_ref().unwrap();
    let notice_lower = notice.to_lowercase();
    assert!(
        notice_lower.contains("autônomo") || notice_lower.contains("autonomous"),
        "Notice should mention Autonomous, got: '{}'",
        notice
    );
    assert_eq!(
        app.mode,
        Mode::Assisted,
        "Mode must remain Assisted until task is submitted"
    );
}

#[test]
fn f9_autonomous_after_task_submission_enables_mode() {
    let mut app = App::new(PathBuf::from("/test/repo"));
    let session = SessionId::new("sess-f9-submit");
    app.session_id = Some(session.clone());
    app.mode = Mode::Assisted;

    let ctrl_bracket = make_key(KeyCode::Char(']'), KeyModifiers::CONTROL);

    // 1. Toggle mode without a task -> gates on task prompt
    handle_key(&mut app, ctrl_bracket);
    let act = handle_key(&mut app, plain_key(KeyCode::Char('m')));
    assert_eq!(act, AppAction::None);
    assert!(matches!(app.ui_mode, UiMode::Palette { .. }));

    // 2. Drive key events to type task description
    let task_text = "refactor routing module";
    for c in task_text.chars() {
        let char_act = handle_key(&mut app, plain_key(KeyCode::Char(c)));
        assert_eq!(char_act, AppAction::None);
    }

    // 3. Press Enter to submit task
    let submit_act = handle_key(&mut app, plain_key(KeyCode::Enter));

    // Must emit both SubmitTask and SetMode to Autonomous
    assert_eq!(
        submit_act,
        AppAction::SendMessages(vec![
            ClientMessage::SubmitTask {
                session_id: session.clone(),
                task: task_text.to_string(),
            },
            ClientMessage::SetMode {
                session_id: session.clone(),
                mode: Mode::Autonomous,
            },
        ]),
        "Submitting task must emit SubmitTask and SetMode to Autonomous"
    );

    assert_eq!(app.mode, Mode::Autonomous, "Mode must now be Autonomous");
    assert!(app.has_task(), "Task must now be stored");
    assert_eq!(app.ui_mode, UiMode::Normal, "UI mode must return to Normal");

    // 4. With a task already stored, toggle behaves as today (switches to Assisted)
    handle_key(&mut app, ctrl_bracket);
    let toggle_to_assisted = handle_key(&mut app, plain_key(KeyCode::Char('m')));
    assert_eq!(
        toggle_to_assisted,
        AppAction::SendMessage(ClientMessage::SetMode {
            session_id: session.clone(),
            mode: Mode::Assisted,
        })
    );
    app.mode = Mode::Assisted;

    // 5. Toggling back to Autonomous with task already stored does NOT prompt again
    handle_key(&mut app, ctrl_bracket);
    let toggle_to_auto = handle_key(&mut app, plain_key(KeyCode::Char('m')));
    assert_eq!(
        toggle_to_auto,
        AppAction::SendMessage(ClientMessage::SetMode {
            session_id: session.clone(),
            mode: Mode::Autonomous,
        })
    );
    assert_eq!(app.ui_mode, UiMode::Normal);

    // 6. Normal typing stays plain PTY input
    let pty_act = handle_key(&mut app, plain_key(KeyCode::Char('x')));
    assert_eq!(pty_act, AppAction::SendPtyInput(b"x".to_vec()));
}

#[test]
fn r3_router_hold_deadline_through_daemon_message_and_tui_accept() {
    use aihub::handle_daemon_msg;
    use aihub::keys::handle_key_at;
    use aihub_core::DaemonMessage;
    use aihub_core::TaskSize;
    use aihub_router::route_outcome_at;

    let now_s = 1_000_000u64;
    let agy = aihub_core::QuotaSnapshot {
        slot: aihub_core::SlotId::default_for(HarnessId::Antigravity),
        status: aihub_core::QuotaStatus::Ok,
        source: aihub_core::QuotaSource::Vendor,
        estimated: false,
        note: None,
        windows: vec![],
        lanes: vec![aihub_core::QuotaLane {
            name: "gemini".into(),
            kind: aihub_core::LaneKind::Own,
            windows: vec![aihub_core::QuotaWindow::new(
                aihub_core::WindowKind::FiveHour,
                85.,
                Some(30),
                Some(18_000),
            )],
        }],
    };
    let catalog = aihub_router::ModelCatalog::from_models(vec![], vec!["gemini-test".into()]);
    let outcome = route_outcome_at(
        TaskTier::Mechanical,
        TaskSize::S,
        &[agy],
        7200,
        &catalog,
        now_s,
    )
    .unwrap();
    let holds_until = outcome
        .holds_until_s()
        .expect("router must emit hold deadline");
    assert_eq!(
        holds_until,
        now_s + 30,
        "wire hold must be epoch deadline, not duration"
    );

    let mut app = App::new(PathBuf::from("/test/repo"));
    let session = SessionId::new("r3-hold");
    app.session_id = Some(session.clone());
    app.mode = Mode::Assisted;

    handle_daemon_msg(
        &mut app,
        DaemonMessage::RouteRecommendation {
            session_id: session.clone(),
            outcome: outcome.clone(),
            recommendation_id: 7,
        },
    );

    let ctrl_bracket = make_key(KeyCode::Char(']'), KeyModifiers::CONTROL);
    handle_key_at(&mut app, ctrl_bracket, now_s);
    let blocked = handle_key_at(&mut app, plain_key(KeyCode::Enter), now_s);
    assert_eq!(
        blocked,
        AppAction::None,
        "TUI must treat holds_until_s as epoch seconds, not duration"
    );

    handle_key_at(&mut app, ctrl_bracket, holds_until);
    let accept = handle_key_at(&mut app, plain_key(KeyCode::Enter), holds_until);
    assert_eq!(
        accept,
        AppAction::SendMessage(ClientMessage::AcceptRecommendation {
            session_id: session,
            recommendation_id: Some(7),
        }),
        "after the router deadline, assisted accept must not send SwitchHarness"
    );
}

#[test]
fn hold_assisted_enter_refuses_until_hold_expires() {
    use aihub::keys::handle_key_at;

    let mut app = App::new(PathBuf::from("/test/repo"));
    let session = SessionId::new("sess-hold");
    app.session_id = Some(session.clone());
    app.mode = Mode::Assisted;

    // Recommendation with a hold until epoch seconds 50400 (14:00 UTC)
    let hold_target_s = 50400u64;
    app.recommendation = Some(RecommendationState::Recommended {
        tier: TaskTier::Mechanical,
        harness: HarnessId::Antigravity,
        lane: Some("gemini".to_string()),
        model: Some("gemini-recommended".to_string()),
        holds_until_s: Some(hold_target_s),
        confidence: 0.95,
        reason: "Fast mechanical refactor".to_string(),
        recommendation_id: 1,
    });

    let ctrl_bracket = make_key(KeyCode::Char(']'), KeyModifiers::CONTROL);

    // 1. Clock is in the past: now = 50000 < 50400
    let now_before = 50000u64;
    handle_key_at(&mut app, ctrl_bracket, now_before);
    assert!(app.prefix_active);

    let action_before = handle_key_at(&mut app, plain_key(KeyCode::Enter), now_before);
    assert_eq!(
        action_before,
        AppAction::None,
        "Accepting recommendation while hold is in the future must emit no message"
    );
    assert!(
        app.status_message.is_some(),
        "Status notice must be shown when hold is active"
    );
    let (notice, _) = app.status_message.as_ref().unwrap();
    assert!(
        notice.contains("held until 14:00"),
        "Notice should show 'held until HH:MM', got: '{}'",
        notice
    );

    // 2. Clock reaches or passes the hold: now = 50400
    handle_key_at(&mut app, ctrl_bracket, hold_target_s);
    assert!(app.prefix_active);

    let action_after = handle_key_at(&mut app, plain_key(KeyCode::Enter), hold_target_s);
    assert_eq!(
        action_after,
        AppAction::SendMessage(ClientMessage::AcceptRecommendation {
            session_id: session.clone(),
            recommendation_id: Some(1),
        }),
        "Accepting recommendation once hold has expired must send AcceptRecommendation"
    );
}
