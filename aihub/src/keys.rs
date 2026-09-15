//! Keystroke routing, prefix chord processing, and palette command handling.

use crate::state::{App, RecommendationState, UiMode};
use aihub_core::{ClientMessage, HarnessId, MergeStrategy, Mode, QuotaSnapshot, QuotaStatus};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Action resulting from processing a key event.
#[derive(Debug, Clone, PartialEq)]
pub enum AppAction {
    None,
    SendPtyInput(Vec<u8>),
    SendMessage(ClientMessage),
    SetUiMode(UiMode),
    Exit,
}

/// Cycles harness in order: ClaudeCode -> Antigravity -> Codex -> CursorAgent -> ClaudeCode.
pub fn cycle_harness(current: HarnessId) -> HarnessId {
    let all = HarnessId::all();
    let idx = all.iter().position(|&h| h == current).unwrap_or(0);
    all[(idx + 1) % all.len()]
}

/// Cycles only harnesses whose slot has supply, meaning status is not Empty or Unknown.
/// Falls back to cycling all harnesses if no snapshots or supply information is available.
pub fn cycle_available_harness(current: HarnessId, snapshots: &[QuotaSnapshot]) -> HarnessId {
    let available: Vec<HarnessId> = HarnessId::all()
        .iter()
        .copied()
        .filter(|&h| {
            snapshots.iter().any(|s| {
                s.slot.harness == h
                    && s.status != QuotaStatus::Empty
                    && s.status != QuotaStatus::Unknown
            })
        })
        .collect();

    if available.is_empty() {
        return cycle_harness(current);
    }

    if let Some(idx) = available.iter().position(|&h| h == current) {
        available[(idx + 1) % available.len()]
    } else {
        available[0]
    }
}

/// Converts a crossterm KeyEvent into raw byte sequence suitable for a Unix PTY.
pub fn key_event_to_bytes(key: KeyEvent) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    let mut bytes = match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                let code = match c {
                    'a'..='z' => c as u8 - b'a' + 1,
                    'A'..='Z' => c as u8 - b'A' + 1,
                    '@' | ' ' => 0,
                    '[' => 0x1B,
                    '\\' => 0x1C,
                    ']' => 0x1D,
                    '^' => 0x1E,
                    '_' => 0x1F,
                    '?' => 0x7F,
                    _ => return None,
                };
                vec![code]
            } else {
                let mut buf = [0u8; 4];
                let s = c.encode_utf8(&mut buf);
                s.as_bytes().to_vec()
            }
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7F],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Esc => vec![0x1B],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::F(1) => b"\x1bOP".to_vec(),
        KeyCode::F(2) => b"\x1bOQ".to_vec(),
        KeyCode::F(3) => b"\x1bOR".to_vec(),
        KeyCode::F(4) => b"\x1bOS".to_vec(),
        KeyCode::F(5) => b"\x1b[15~".to_vec(),
        KeyCode::F(6) => b"\x1b[17~".to_vec(),
        KeyCode::F(7) => b"\x1b[18~".to_vec(),
        KeyCode::F(8) => b"\x1b[19~".to_vec(),
        KeyCode::F(9) => b"\x1b[20~".to_vec(),
        KeyCode::F(10) => b"\x1b[21~".to_vec(),
        KeyCode::F(11) => b"\x1b[23~".to_vec(),
        KeyCode::F(12) => b"\x1b[24~".to_vec(),
        _ => return None,
    };

    if alt && !ctrl {
        let mut with_alt = vec![0x1B];
        with_alt.extend(bytes);
        bytes = with_alt;
    }

    Some(bytes)
}

/// Main key routing function for the application.
pub fn handle_key(app: &mut App, key: KeyEvent) -> AppAction {
    match &mut app.ui_mode {
        UiMode::Normal => handle_normal_key(app, key),
        UiMode::Palette { .. } => handle_palette_key(app, key),
        UiMode::QuotaTable { .. } => handle_quota_table_key(app, key),
        UiMode::MergeReview { .. } => handle_merge_review_key(app, key),
    }
}

fn handle_normal_key(app: &mut App, key: KeyEvent) -> AppAction {
    let is_ctrl_bracket = (key.code == KeyCode::Char(']')
        && key.modifiers.contains(KeyModifiers::CONTROL))
        || (key.code == KeyCode::Char('\x1d'));

    if is_ctrl_bracket {
        if app.prefix_active {
            // Already in prefix mode: send literal Ctrl+] (0x1D) to PTY
            app.prefix_active = false;
            return AppAction::SendPtyInput(vec![0x1D]);
        } else {
            // Activate prefix chord
            app.prefix_active = true;
            return AppAction::None;
        }
    }

    if app.prefix_active {
        // Any key after prefix consumes prefix mode
        app.prefix_active = false;
        match key.code {
            KeyCode::Char('p') | KeyCode::Char(':') => {
                app.ui_mode = UiMode::Palette {
                    input: "/".to_string(),
                    selected_index: 0,
                };
                AppAction::None
            }
            KeyCode::Enter => {
                // Accept assisted-mode recommendation
                match &app.recommendation {
                    Some(RecommendationState::Recommended { harness, .. }) => {
                        if let Some(session_id) = &app.session_id {
                            return AppAction::SendMessage(ClientMessage::SwitchHarness {
                                session_id: session_id.clone(),
                                target: *harness,
                                with_handoff: true,
                            });
                        }
                    }
                    Some(RecommendationState::NoCapacity { reason }) => {
                        app.set_status(format!("Não é possível aceitar: {}", reason));
                        return AppAction::None;
                    }
                    None => {}
                }
                AppAction::None
            }
            KeyCode::Tab => {
                // Cycle harness only over available harnesses with supply
                let next = cycle_available_harness(app.harness, &app.snapshots);
                if let Some(session_id) = &app.session_id {
                    if next != app.harness {
                        AppAction::SendMessage(ClientMessage::SwitchHarness {
                            session_id: session_id.clone(),
                            target: next,
                            with_handoff: true,
                        })
                    } else {
                        app.set_status(format!(
                            "Apenas {} possui capacidade disponível",
                            app.harness.binary_name()
                        ));
                        AppAction::None
                    }
                } else {
                    AppAction::None
                }
            }
            KeyCode::Char('m') => {
                // Toggle mode
                let next_mode = match app.mode {
                    Mode::Assisted => Mode::Autonomous,
                    Mode::Autonomous => Mode::Assisted,
                };
                if let Some(session_id) = &app.session_id {
                    AppAction::SendMessage(ClientMessage::SetMode {
                        session_id: session_id.clone(),
                        mode: next_mode,
                    })
                } else {
                    AppAction::None
                }
            }
            KeyCode::Char('q') => {
                // Quota table
                app.ui_mode = UiMode::QuotaTable { scroll: 0 };
                AppAction::None
            }
            KeyCode::Char('d') => {
                // Detach
                if let Some(session_id) = &app.session_id {
                    AppAction::SendMessage(ClientMessage::Detach {
                        session_id: session_id.clone(),
                    })
                } else {
                    AppAction::Exit
                }
            }
            _ => {
                // Any other key simply cancels prefix mode
                AppAction::None
            }
        }
    } else {
        // Normal PTY pass-through
        if let Some(bytes) = key_event_to_bytes(key) {
            AppAction::SendPtyInput(bytes)
        } else {
            AppAction::None
        }
    }
}

fn handle_palette_key(app: &mut App, key: KeyEvent) -> AppAction {
    let input = match &mut app.ui_mode {
        UiMode::Palette { input, .. } => input.clone(),
        _ => return AppAction::None,
    };

    match key.code {
        KeyCode::Esc => {
            app.ui_mode = UiMode::Normal;
            AppAction::None
        }
        KeyCode::Enter => execute_palette_command(app, &input),
        KeyCode::Backspace => {
            if let UiMode::Palette { input, .. } = &mut app.ui_mode {
                input.pop();
                if input.is_empty() {
                    // Leaving palette if backspaced empty
                    app.ui_mode = UiMode::Normal;
                }
            }
            AppAction::None
        }
        KeyCode::Char(c) => {
            if let UiMode::Palette { input, .. } = &mut app.ui_mode {
                input.push(c);
            }
            AppAction::None
        }
        KeyCode::Tab => {
            // Autocomplete palette command
            autocomplete_palette(app);
            AppAction::None
        }
        _ => AppAction::None,
    }
}

const PALETTE_COMMANDS: &[&str] = &[
    "/task <descrição>",
    "/switch agy",
    "/switch claude",
    "/switch codex",
    "/switch cursor-agent",
    "/merge",
    "/quota",
    "/mode",
    "/detach",
];

fn autocomplete_palette(app: &mut App) {
    if let UiMode::Palette { input, .. } = &mut app.ui_mode {
        let trimmed = input.trim();
        for cmd in PALETTE_COMMANDS {
            if cmd.starts_with(trimmed) && *cmd != trimmed {
                *input = cmd.to_string();
                return;
            }
        }
    }
}

/// Executes command from palette input:
/// - `/switch <harness>`
/// - `/merge`
/// - `/quota`
/// - `/mode`
/// - `/detach`
pub fn execute_palette_command(app: &mut App, raw_cmd: &str) -> AppAction {
    let cmd = raw_cmd.trim();
    let normalized = if let Some(stripped) = cmd.strip_prefix('/') {
        stripped.trim()
    } else if let Some(stripped) = cmd.strip_prefix(':') {
        stripped.trim()
    } else {
        cmd
    };

    let mut parts = normalized.split_whitespace();
    let verb = parts.next().unwrap_or("");

    match verb {
        "task" => {
            let task_text = normalized
                .strip_prefix("task")
                .unwrap_or("")
                .trim()
                .to_string();
            app.ui_mode = UiMode::Normal;
            if task_text.is_empty() {
                app.set_status("Uso: /task <texto da tarefa>");
                AppAction::None
            } else if let Some(session_id) = app.session_id.clone() {
                app.set_status(format!("Tarefa enviada: {}", task_text));
                AppAction::SendMessage(ClientMessage::SubmitTask {
                    session_id,
                    task: task_text,
                })
            } else {
                app.set_status("Nenhuma sessão ativa para submeter tarefa");
                AppAction::None
            }
        }
        "switch" => {
            let target_str = parts.next().unwrap_or("");
            let harness = match target_str {
                "claude" | "claude-code" => Some(HarnessId::ClaudeCode),
                "agy" | "antigravity" => Some(HarnessId::Antigravity),
                "codex" => Some(HarnessId::Codex),
                "cursor" | "cursor-agent" => Some(HarnessId::CursorAgent),
                _ => HarnessId::from_binary_name(target_str),
            };

            app.ui_mode = UiMode::Normal;
            if let Some(target) = harness {
                if let Some(session_id) = &app.session_id {
                    AppAction::SendMessage(ClientMessage::SwitchHarness {
                        session_id: session_id.clone(),
                        target,
                        with_handoff: true,
                    })
                } else {
                    app.set_status("Nenhuma sessão ativa para alternar harness");
                    AppAction::None
                }
            } else {
                app.set_status(format!(
                    "Harness desconhecido: '{}'. Opções: agy, claude, codex, cursor-agent",
                    target_str
                ));
                AppAction::None
            }
        }
        "merge" => {
            app.ui_mode = UiMode::Normal;
            if let Some(session_id) = &app.session_id {
                // Sends initial MergeRequest to retrieve review diff from daemon
                AppAction::SendMessage(ClientMessage::MergeRequest {
                    session_id: session_id.clone(),
                    strategy: MergeStrategy::Squash,
                })
            } else {
                app.set_status("Nenhuma sessão ativa para merge");
                AppAction::None
            }
        }
        "quota" => {
            app.ui_mode = UiMode::QuotaTable { scroll: 0 };
            AppAction::None
        }
        "mode" => {
            app.ui_mode = UiMode::Normal;
            let next_mode = match app.mode {
                Mode::Assisted => Mode::Autonomous,
                Mode::Autonomous => Mode::Assisted,
            };
            if let Some(session_id) = &app.session_id {
                AppAction::SendMessage(ClientMessage::SetMode {
                    session_id: session_id.clone(),
                    mode: next_mode,
                })
            } else {
                AppAction::None
            }
        }
        "detach" => {
            app.ui_mode = UiMode::Normal;
            if let Some(session_id) = &app.session_id {
                AppAction::SendMessage(ClientMessage::Detach {
                    session_id: session_id.clone(),
                })
            } else {
                AppAction::Exit
            }
        }
        "" => {
            app.ui_mode = UiMode::Normal;
            AppAction::None
        }
        _ => {
            app.ui_mode = UiMode::Normal;
            app.set_status(format!("Comando desconhecido: /{}", verb));
            AppAction::None
        }
    }
}

fn handle_quota_table_key(app: &mut App, key: KeyEvent) -> AppAction {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter => {
            app.ui_mode = UiMode::Normal;
            AppAction::None
        }
        KeyCode::Up => {
            if let UiMode::QuotaTable { scroll } = &mut app.ui_mode {
                *scroll = scroll.saturating_sub(1);
            }
            AppAction::None
        }
        KeyCode::Down => {
            if let UiMode::QuotaTable { scroll } = &mut app.ui_mode {
                *scroll += 1;
            }
            AppAction::None
        }
        _ => AppAction::None,
    }
}

fn handle_merge_review_key(app: &mut App, key: KeyEvent) -> AppAction {
    let current_strategy = match &app.ui_mode {
        UiMode::MergeReview { strategy, .. } => *strategy,
        _ => return AppAction::None,
    };

    match key.code {
        KeyCode::Esc => {
            app.ui_mode = UiMode::Normal;
            AppAction::None
        }
        KeyCode::Char('s') => {
            if let UiMode::MergeReview { strategy, .. } = &mut app.ui_mode {
                *strategy = MergeStrategy::Squash;
            }
            if let Some(session_id) = &app.session_id {
                AppAction::SendMessage(ClientMessage::MergeRequest {
                    session_id: session_id.clone(),
                    strategy: MergeStrategy::Squash,
                })
            } else {
                AppAction::None
            }
        }
        KeyCode::Char('f') => {
            if let UiMode::MergeReview { strategy, .. } = &mut app.ui_mode {
                *strategy = MergeStrategy::FastForward;
            }
            if let Some(session_id) = &app.session_id {
                AppAction::SendMessage(ClientMessage::MergeRequest {
                    session_id: session_id.clone(),
                    strategy: MergeStrategy::FastForward,
                })
            } else {
                AppAction::None
            }
        }
        KeyCode::Char('k') => {
            if let UiMode::MergeReview { strategy, .. } = &mut app.ui_mode {
                *strategy = MergeStrategy::Keep;
            }
            if let Some(session_id) = &app.session_id {
                AppAction::SendMessage(ClientMessage::MergeRequest {
                    session_id: session_id.clone(),
                    strategy: MergeStrategy::Keep,
                })
            } else {
                AppAction::None
            }
        }
        KeyCode::Char('d') => {
            if let UiMode::MergeReview { strategy, .. } = &mut app.ui_mode {
                *strategy = MergeStrategy::Discard;
            }
            if let Some(session_id) = &app.session_id {
                AppAction::SendMessage(ClientMessage::MergeRequest {
                    session_id: session_id.clone(),
                    strategy: MergeStrategy::Discard,
                })
            } else {
                AppAction::None
            }
        }
        KeyCode::Enter => {
            // Confirm with the current strategy
            if let Some(session_id) = &app.session_id {
                AppAction::SendMessage(ClientMessage::MergeRequest {
                    session_id: session_id.clone(),
                    strategy: current_strategy,
                })
            } else {
                AppAction::None
            }
        }
        KeyCode::Up => {
            if let UiMode::MergeReview { scroll, .. } = &mut app.ui_mode {
                *scroll = scroll.saturating_sub(1);
            }
            AppAction::None
        }
        KeyCode::Down => {
            if let UiMode::MergeReview { scroll, .. } = &mut app.ui_mode {
                *scroll += 1;
            }
            AppAction::None
        }
        KeyCode::PageUp => {
            if let UiMode::MergeReview { scroll, .. } = &mut app.ui_mode {
                *scroll = scroll.saturating_sub(10);
            }
            AppAction::None
        }
        KeyCode::PageDown => {
            if let UiMode::MergeReview { scroll, .. } = &mut app.ui_mode {
                *scroll += 10;
            }
            AppAction::None
        }
        _ => AppAction::None,
    }
}
