//! Keystroke routing, prefix chord, and palette execution tests.

use std::path::PathBuf;
use aihub_core::{
    ClientMessage, HarnessId, MergeStrategy, Mode, SessionId, TaskTier,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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
    assert_eq!(key_event_to_bytes(plain_key(KeyCode::Char('z'))), Some(b"z".to_vec()));
    assert_eq!(key_event_to_bytes(plain_key(KeyCode::Esc)), Some(vec![0x1B]));

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
    let action = handle_key(&mut app, make_key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert_eq!(action, AppAction::SendPtyInput(vec![3]));

    // Ctrl+D
    let action = handle_key(&mut app, make_key(KeyCode::Char('d'), KeyModifiers::CONTROL));
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
    assert_eq!(action, AppAction::None, "Prefix chord should not be sent to PTY");
    assert!(app.prefix_active, "Prefix chord should be marked active");
}

#[test]
fn test_prefix_actions() {
    let mut app = App::new(PathBuf::from("/test/repo"));
    let session = SessionId::new("test-session");
    app.session_id = Some(session.clone());
    app.harness = HarnessId::ClaudeCode;
    app.mode = Mode::Assisted;
    app.recommendation = Some(RecommendationState {
        tier: TaskTier::Mechanical,
        harness: HarnessId::Antigravity,
        lane: None,
        holds_until_s: None,
        confidence: 0.9,
        reason: "mechanical task".to_string(),
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
        AppAction::SendMessage(ClientMessage::SwitchHarness {
            session_id: session.clone(),
            target: HarnessId::Antigravity,
            with_handoff: true,
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

    // Test /switch agy
    let action = aihub::keys::execute_palette_command(&mut app, "/switch agy");
    assert_eq!(
        action,
        AppAction::SendMessage(ClientMessage::SwitchHarness {
            session_id: session.clone(),
            target: HarnessId::Antigravity,
            with_handoff: true,
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
