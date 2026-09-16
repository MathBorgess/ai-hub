//! aihub library — Interactive multi-harness terminal supervisor and TUI client.

pub mod cli;
pub mod colors;
pub mod connection;
pub mod keys;
pub mod state;
pub mod terminal;
pub mod ui;

use aihub_core::{
    ClientMessage, DaemonMessage, HarnessId, MergeStrategy, SessionId, SessionTarget,
};
use anyhow::{Context, Result};
use crossterm::event::Event;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Position;
use ratatui::Terminal;
use std::io::stdout;
use std::time::Duration;
use tokio::sync::mpsc;

use crate::cli::{detect_repo_path, Cli, Commands};
use crate::connection::{
    connect_or_start_daemon, perform_handshake, recv_msg, send_msg, spawn_daemon_reader,
};
use crate::keys::{handle_key, AppAction};
use crate::state::{App, RecommendationState, UiMode};
use crate::terminal::{setup_panic_hook, shutdown_signal, TerminalGuard};

/// Main application runner.
pub async fn run(cli: Cli) -> Result<()> {
    setup_panic_hook();

    let socket_path = cli.socket.unwrap_or_else(aihub_core::default_socket_path);
    let repo_path = detect_repo_path();

    // 1. Connect or start daemon
    let stream = connect_or_start_daemon(&socket_path)
        .await
        .context("Could not connect to aihubd")?;
    let (mut reader, mut writer) = stream.into_split();

    // 2. Perform protocol handshake
    perform_handshake(&mut writer, &mut reader).await?;

    let mut app = App::new(repo_path.clone());

    // 3. Attach or create session based on command
    match cli.command {
        Some(Commands::Attach { session_id }) => {
            let target = match session_id {
                Some(id) => SessionTarget::Id(SessionId::new(id)),
                None => SessionTarget::LatestForRepo(repo_path),
            };
            send_msg(&mut writer, &ClientMessage::Attach { target }).await?;

            // Read daemon response for attach
            if let Ok(Ok(msg)) =
                tokio::time::timeout(Duration::from_secs(3), recv_msg(&mut reader)).await
            {
                handle_daemon_msg(&mut app, msg);
            }
        }
        None => {
            // Bare `aihub`: attach to latest for repo, or create new session
            send_msg(
                &mut writer,
                &ClientMessage::Attach {
                    target: SessionTarget::LatestForRepo(repo_path.clone()),
                },
            )
            .await?;

            // Read next daemon response
            let resp = tokio::time::timeout(Duration::from_secs(3), recv_msg(&mut reader)).await;
            match resp {
                Ok(Ok(DaemonMessage::Attached {
                    session_id,
                    scrollback,
                    summary,
                })) => {
                    app.session_id = Some(session_id);
                    app.harness = summary.harness;
                    app.branch = summary.branch;
                    app.mode = summary.mode;
                    app.active = summary.active;
                    app.worktree_path = summary.worktree_path;
                    app.repo_path = summary.repo_path;
                    app.vt_parser.process(scrollback.as_slice());
                }
                Ok(Ok(DaemonMessage::QuotaPush { snapshots })) => {
                    app.snapshots = snapshots;
                    // Try waiting for next message (Attached or Error)
                    if let Ok(Ok(next)) =
                        tokio::time::timeout(Duration::from_secs(2), recv_msg(&mut reader)).await
                    {
                        match next {
                            DaemonMessage::Attached {
                                session_id,
                                scrollback,
                                summary,
                            } => {
                                app.session_id = Some(session_id);
                                app.harness = summary.harness;
                                app.branch = summary.branch;
                                app.mode = summary.mode;
                                app.active = summary.active;
                                app.worktree_path = summary.worktree_path;
                                app.repo_path = summary.repo_path;
                                app.vt_parser.process(scrollback.as_slice());
                            }
                            DaemonMessage::Error { .. } => {
                                // No existing session; create new session
                                create_new_session(
                                    &mut writer,
                                    &mut reader,
                                    &mut app,
                                    &repo_path,
                                    cli.task.clone(),
                                )
                                .await?;
                            }
                            other => handle_daemon_msg(&mut app, other),
                        }
                    }
                }
                Ok(Ok(DaemonMessage::Error { .. })) | Err(_) => {
                    // Create new session
                    create_new_session(
                        &mut writer,
                        &mut reader,
                        &mut app,
                        &repo_path,
                        cli.task.clone(),
                    )
                    .await?;
                }
                Ok(Ok(other)) => {
                    handle_daemon_msg(&mut app, other);
                }
                Ok(Err(e)) => return Err(e),
            }
        }
    }

    // 4. Set up terminal raw mode & alternate screen
    let mut term_guard = TerminalGuard::new()?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    // Sync initial terminal size to PTY
    let term_size = terminal.size()?;
    app.last_terminal_size = (term_size.width, term_size.height);
    app.vt_parser.set_size(term_size.height, term_size.width);
    if let Some(session_id) = &app.session_id {
        let _ = send_msg(
            &mut writer,
            &ClientMessage::PtyResize {
                session_id: session_id.clone(),
                cols: term_size.width,
                rows: term_size.height,
            },
        )
        .await;
    }

    // 5. Input event channel (blocking read in separate thread)
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let _input_thread = std::thread::spawn(move || {
        while let Ok(event) = crossterm::event::read() {
            if event_tx.send(event).is_err() {
                break;
            }
        }
    });

    // Spawn dedicated daemon reader task (F8)
    let mut daemon_rx = spawn_daemon_reader(reader);

    // Request fresh quotas from daemon
    let _ = send_msg(&mut writer, &ClientMessage::RequestQuota).await;

    // Submit initial task if provided via CLI argument (F9)
    if let Some(task_text) = &cli.task {
        app.task = Some(task_text.clone());
        if let Some(session_id) = &app.session_id {
            let _ = send_msg(
                &mut writer,
                &ClientMessage::SubmitTask {
                    session_id: session_id.clone(),
                    task: task_text.clone(),
                },
            )
            .await;
        }
    }

    // 6. Main event loop
    loop {
        app.clear_expired_status(Duration::from_secs(5));

        // Check if resize needed
        let current_size = terminal.size()?;
        if (current_size.width, current_size.height) != app.last_terminal_size {
            app.last_terminal_size = (current_size.width, current_size.height);
            let body_rows = current_size.height.saturating_sub(3).max(1);
            app.vt_parser.set_size(body_rows, current_size.width);
            if let Some(session_id) = &app.session_id {
                let _ = send_msg(
                    &mut writer,
                    &ClientMessage::PtyResize {
                        session_id: session_id.clone(),
                        cols: current_size.width,
                        rows: body_rows,
                    },
                )
                .await;
            }
        }

        // Draw UI frame
        let mut cursor_pos = None;
        terminal.draw(|frame| {
            let area = frame.area();
            cursor_pos = ui::render_app(&app, area, frame.buffer_mut());
            if let Some((cx, cy)) = cursor_pos {
                frame.set_cursor_position(Position::new(cx, cy));
            }
        })?;

        if app.should_exit {
            break;
        }

        tokio::select! {
            // Signal received (SIGTERM, SIGINT)
            _ = shutdown_signal() => {
                break;
            }

            // Input event from crossterm thread
            Some(event) = event_rx.recv() => {
                match event {
                    Event::Key(key) => {
                        let action = handle_key(&mut app, key);
                        match action {
                            AppAction::SendPtyInput(bytes) => {
                                if let Some(session_id) = &app.session_id {
                                    let _ = send_msg(
                                        &mut writer,
                                        &ClientMessage::PtyInput {
                                            session_id: session_id.clone(),
                                            data: bytes.into(),
                                        },
                                    )
                                    .await;
                                }
                            }
                            AppAction::SendMessage(msg) => {
                                let _ = send_msg(&mut writer, &msg).await;
                            }
                            AppAction::SendMessages(msgs) => {
                                for msg in msgs {
                                    let _ = send_msg(&mut writer, &msg).await;
                                }
                            }
                            AppAction::SetUiMode(mode) => {
                                app.ui_mode = mode;
                            }
                            AppAction::Exit => {
                                break;
                            }
                            AppAction::None => {}
                        }
                    }
                    Event::Resize(cols, rows) => {
                        let body_rows = rows.saturating_sub(3).max(1);
                        app.vt_parser.set_size(body_rows, cols);
                        app.last_terminal_size = (cols, rows);
                        if let Some(session_id) = &app.session_id {
                            let _ = send_msg(
                                &mut writer,
                                &ClientMessage::PtyResize {
                                    session_id: session_id.clone(),
                                    cols,
                                    rows: body_rows,
                                },
                            )
                            .await;
                        }
                    }
                    _ => {}
                }
            }

            // Message from daemon via dedicated reader task (F8)
            msg_opt = daemon_rx.recv() => {
                match msg_opt {
                    Some(Ok(msg)) => {
                        handle_daemon_msg(&mut app, msg);
                    }
                    Some(Err(_)) | None => {
                        // Connection lost: attempt reconnection
                        app.set_status("Conexão perdida. Tentando reconectar...");
                        let mut reconnected = false;
                        for _ in 0..10 {
                            tokio::time::sleep(Duration::from_millis(500)).await;
                            if let Ok(new_stream) = connect_or_start_daemon(&socket_path).await {
                                let (mut new_reader, mut new_writer) = new_stream.into_split();
                                if perform_handshake(&mut new_writer, &mut new_reader).await.is_ok() {
                                    if let Some(session_id) = &app.session_id {
                                        let _ = send_msg(
                                            &mut new_writer,
                                            &ClientMessage::Attach {
                                                target: SessionTarget::Id(session_id.clone()),
                                            },
                                        )
                                        .await;
                                    }
                                    daemon_rx = spawn_daemon_reader(new_reader);
                                    writer = new_writer;
                                    reconnected = true;
                                    app.set_status("Reconectado ao daemon com sucesso.");
                                    break;
                                }
                            }
                        }
                        if !reconnected {
                            app.set_status("Falha ao reconectar ao aihubd.");
                            break;
                        }
                    }
                }
            }
        }
    }

    // Clean exit: restore terminal
    term_guard.restore();
    Ok(())
}

async fn create_new_session(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    reader: &mut tokio::net::unix::OwnedReadHalf,
    app: &mut App,
    repo_path: &std::path::Path,
    initial_prompt: Option<String>,
) -> Result<()> {
    app.task = initial_prompt.clone();
    send_msg(
        writer,
        &ClientMessage::NewSession {
            harness: HarnessId::ClaudeCode,
            repo_path: repo_path.to_path_buf(),
            initial_prompt,
        },
    )
    .await?;

    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        match tokio::time::timeout(Duration::from_secs(3), recv_msg(reader)).await {
            Ok(Ok(DaemonMessage::SessionCreated {
                session_id,
                harness,
                worktree_path,
                branch,
            })) => {
                app.session_id = Some(session_id.clone());
                app.harness = harness;
                app.worktree_path = worktree_path;
                app.branch = branch;

                // Now attach to it
                send_msg(
                    writer,
                    &ClientMessage::Attach {
                        target: SessionTarget::Id(session_id),
                    },
                )
                .await?;
                break;
            }
            Ok(Ok(DaemonMessage::Attached {
                session_id,
                scrollback,
                summary,
            })) => {
                app.session_id = Some(session_id);
                app.harness = summary.harness;
                app.branch = summary.branch;
                app.mode = summary.mode;
                app.active = summary.active;
                app.worktree_path = summary.worktree_path;
                app.repo_path = summary.repo_path;
                app.vt_parser.process(scrollback.as_slice());
                break;
            }
            Ok(Ok(other)) => {
                handle_daemon_msg(app, other);
            }
            Ok(Err(e)) => return Err(e),
            Err(_) => break,
        }
    }
    Ok(())
}

/// Dispatches an incoming DaemonMessage to the App state.
/// Scoped messages for other sessions are filtered out (Finding F13).
pub fn handle_daemon_msg(app: &mut App, msg: DaemonMessage) {
    match msg {
        DaemonMessage::Attached {
            session_id,
            scrollback,
            summary,
        } => {
            app.session_id = Some(session_id);
            app.harness = summary.harness;
            app.branch = summary.branch;
            app.mode = summary.mode;
            app.active = summary.active;
            app.worktree_path = summary.worktree_path;
            app.repo_path = summary.repo_path;
            app.vt_parser.process(scrollback.as_slice());
        }
        DaemonMessage::Detached { session_id } => {
            if let Some(curr) = &app.session_id {
                if curr != &session_id {
                    return;
                }
            }
            app.should_exit = true;
        }
        DaemonMessage::PtyOutput { session_id, data } => {
            if let Some(curr) = &app.session_id {
                if curr != &session_id {
                    return;
                }
            }
            app.vt_parser.process(data.as_slice());
        }
        DaemonMessage::QuotaPush { snapshots } => {
            app.snapshots = snapshots;
        }
        DaemonMessage::RouteRecommendation {
            session_id,
            outcome,
        } => {
            if let Some(curr) = &app.session_id {
                if curr != &session_id {
                    return;
                }
            }
            match outcome {
                aihub_core::RouteOutcome::Recommendation {
                    harness,
                    lane,
                    model,
                    holds_until_s,
                    ..
                } => {
                    app.recommendation = Some(RecommendationState::Recommended {
                        tier: aihub_core::TaskTier::Design,
                        harness,
                        lane,
                        model,
                        holds_until_s,
                        confidence: 1.0,
                        reason: String::new(),
                    });
                }
                aihub_core::RouteOutcome::NoCapacity { reason } => {
                    app.recommendation = Some(RecommendationState::NoCapacity { reason });
                }
            }
        }
        DaemonMessage::HarnessSwitched {
            session_id,
            new_harness,
            ..
        } => {
            if let Some(curr) = &app.session_id {
                if curr != &session_id {
                    return;
                }
            }
            app.harness = new_harness;
            app.set_status(format!(
                "Harness alterado para {}",
                new_harness.binary_name()
            ));
        }
        DaemonMessage::ModeSet { session_id, mode } => {
            if let Some(curr) = &app.session_id {
                if curr != &session_id {
                    return;
                }
            }
            app.mode = mode;
            app.set_status(format!("Modo alterado para {:?}", mode));
        }
        DaemonMessage::MergeResult {
            session_id,
            success,
            diff,
            message,
        } => {
            if let Some(curr) = &app.session_id {
                if curr != &session_id {
                    return;
                }
            }
            if !success {
                // First step of merge confirmation: display diff for review while preserving chosen strategy
                let strategy = match &app.ui_mode {
                    UiMode::MergeReview { strategy, .. } => *strategy,
                    _ => MergeStrategy::Squash,
                };
                let scroll = match &app.ui_mode {
                    UiMode::MergeReview { scroll, .. } => *scroll,
                    _ => 0,
                };
                app.ui_mode = UiMode::MergeReview {
                    diff,
                    message,
                    strategy,
                    scroll,
                };
            } else {
                app.ui_mode = UiMode::Normal;
                app.set_status(format!("Merge realizado: {}", message));
            }
        }
        DaemonMessage::SessionExited {
            session_id,
            exit_code,
        } => {
            if let Some(curr) = &app.session_id {
                if curr != &session_id {
                    return;
                }
            }
            app.active = false;
            app.set_status(format!("Sessão encerrada (código: {:?})", exit_code));
        }
        DaemonMessage::Error { code, message } => {
            app.set_status(format!("Erro [{}]: {}", code, message));
        }
        DaemonMessage::SessionList { .. }
        | DaemonMessage::SessionCreated { .. }
        | DaemonMessage::Hello { .. } => {}
    }
}
