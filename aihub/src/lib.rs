//! aihub library — Interactive multi-harness terminal supervisor and TUI client.

pub mod cli;
pub mod colors;
pub mod connection;
pub mod doctor;
pub mod identity;
pub mod keys;
pub mod remote;
pub mod state;
pub mod terminal;
pub mod ui;

use aihub_core::{
    ChannelTicket, ClientMessage, DaemonMessage, HarnessId, MergeStrategy, SessionId,
    SessionSummary, SessionTarget,
};
use anyhow::Result;
use crossterm::event::Event;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Position;
use ratatui::Terminal;
use std::io::stdout;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::cli::{detect_repo_path, Cli, Commands};
use crate::connection::{connect_local, perform_handshake, spawn_daemon_reader, DaemonWriter};
use crate::identity::{default_identity_path, Identity};
use crate::keys::{handle_key, AppAction};
use crate::remote::{Backoff, FailureClass, RemoteError, RemoteTarget};
use crate::state::{App, ConnectionState, RecommendationState, UiMode};
use crate::terminal::{setup_panic_hook, shutdown_signal, TerminalGuard};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Outcome of one connection attempt against the daemon, local or remote.
enum ConnEvent {
    Connected {
        writer: DaemonWriter,
        daemon_rx: mpsc::UnboundedReceiver<Result<DaemonMessage>>,
    },
    PairingRequired {
        code: String,
    },
    Failed {
        text: String,
        attempt: u32,
    },
}

/// PTY dual-channel connection becoming available (ADR contradiction 1, Option B).
enum PtyEvent {
    Ready {
        tx: std::sync::mpsc::Sender<WsMessage>,
        rx: mpsc::UnboundedReceiver<std::result::Result<WsMessage, RemoteError>>,
    },
}

/// Main application runner.
pub async fn run(cli: Cli) -> Result<()> {
    setup_panic_hook();

    if let Some(Commands::Doctor {
        remote: remote_flag,
    }) = &cli.command
    {
        return run_doctor_command(&cli, *remote_flag).await;
    }

    let target = match cli.daemon_target() {
        Some(raw) => remote::parse_daemon_target(&raw),
        None => RemoteTarget::Local(
            cli.socket
                .clone()
                .unwrap_or_else(aihub_core::default_socket_path),
        ),
    };
    let autostart = !cli.no_autostart;
    let identity_path = default_identity_path();
    let repo_path = detect_repo_path();

    let mut app = App::new(repo_path.clone());

    // Terminal comes up immediately, before any connection attempt: the
    // connection banner (Connecting / PairingRequired / Reconnecting) is just
    // another `app.connection` state rendered by the normal draw loop, so
    // nothing here blocks on the network before the TUI is interactive
    // (design doc §2.5 — replaces the old synchronous connect-then-draw order).
    let mut term_guard = TerminalGuard::new()?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    let term_size = terminal.size()?;
    app.last_terminal_size = (term_size.width, term_size.height);
    app.vt_parser.set_size(term_size.height, term_size.width);

    // Input event channel (blocking read in separate thread)
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let _input_thread = std::thread::spawn(move || {
        while let Ok(event) = crossterm::event::read() {
            if event_tx.send(event).is_err() {
                break;
            }
        }
    });

    let (conn_tx, mut conn_rx) = mpsc::unbounded_channel::<ConnEvent>();
    tokio::spawn(reconnect_task(
        target.clone(),
        autostart,
        identity_path.clone(),
        conn_tx.clone(),
    ));

    let (pty_tx_events, mut pty_rx_events) = mpsc::unbounded_channel::<PtyEvent>();

    let mut writer: Option<DaemonWriter> = None;
    let mut daemon_rx: Option<mpsc::UnboundedReceiver<Result<DaemonMessage>>> = None;
    let mut pty_out: Option<std::sync::mpsc::Sender<WsMessage>> = None;
    let mut pty_in: Option<mpsc::UnboundedReceiver<std::result::Result<WsMessage, RemoteError>>> =
        None;
    let mut bootstrapped = false;

    loop {
        app.clear_expired_status(Duration::from_secs(5));

        let current_size = terminal.size()?;
        if (current_size.width, current_size.height) != app.last_terminal_size {
            app.last_terminal_size = (current_size.width, current_size.height);
            let body_rows = current_size.height.saturating_sub(3).max(1);
            app.vt_parser.set_size(body_rows, current_size.width);
            if let (Some(w), Some(session_id)) = (writer.as_mut(), app.session_id.clone()) {
                let _ = w
                    .send(&ClientMessage::PtyResize {
                        session_id,
                        cols: current_size.width,
                        rows: body_rows,
                    })
                    .await;
            }
        }

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
                                if let Some(session_id) = app.session_id.clone() {
                                    if let Some(tx) = pty_out.as_ref() {
                                        let frame = aihub_core::PtyBinaryFrame::new(0, bytes).encode();
                                        let _ = tx.send(WsMessage::Binary(frame));
                                    } else if let Some(w) = writer.as_mut() {
                                        let _ = w
                                            .send(&ClientMessage::PtyInput { session_id, data: bytes.into() })
                                            .await;
                                    }
                                }
                            }
                            AppAction::SendMessage(msg) => {
                                if let Some(w) = writer.as_mut() {
                                    let _ = w.send(&msg).await;
                                }
                            }
                            AppAction::SendMessages(msgs) => {
                                if let Some(w) = writer.as_mut() {
                                    for msg in msgs {
                                        let _ = w.send(&msg).await;
                                    }
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
                        if let (Some(w), Some(session_id)) = (writer.as_mut(), app.session_id.clone()) {
                            let _ = w
                                .send(&ClientMessage::PtyResize { session_id, cols, rows: body_rows })
                                .await;
                        }
                    }
                    _ => {}
                }
            }

            // Message from the active daemon connection, local or remote.
            msg_opt = async {
                match daemon_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match msg_opt {
                    Some(Ok(msg)) => {
                        handle_daemon_msg(&mut app, msg);
                    }
                    Some(Err(_)) | None => {
                        // Connection lost: hand off to a fresh background reconnect task
                        // (design doc §2.5). Its backoff sleep runs in that spawned task,
                        // never here, so this select loop keeps drawing every frame.
                        writer = None;
                        daemon_rx = None;
                        pty_out = None;
                        pty_in = None;
                        app.connection = ConnectionState::Reconnecting {
                            class_text: "Conexão perdida".to_string(),
                            attempt: 1,
                        };
                        tokio::spawn(reconnect_task(
                            target.clone(),
                            autostart,
                            identity_path.clone(),
                            conn_tx.clone(),
                        ));
                    }
                }
            }

            // Connection lifecycle event from the background reconnect task.
            Some(event) = conn_rx.recv() => {
                match event {
                    ConnEvent::Connected { writer: mut new_writer, daemon_rx: mut new_rx } => {
                        app.connection = ConnectionState::Connected;
                        if !bootstrapped {
                            bootstrapped = true;
                            bootstrap_session(&mut new_writer, &mut new_rx, &mut app, &cli, &repo_path).await?;

                            if let RemoteTarget::Remote(url) = &target {
                                if let (Some(ticket), Some(session_id)) =
                                    (app.channel_ticket.clone(), app.session_id.clone())
                                {
                                    spawn_pty_connect(url.clone(), session_id, ticket, pty_tx_events.clone());
                                }
                            }
                        } else if let Some(session_id) = app.session_id.clone() {
                            let _ = new_writer
                                .send(&ClientMessage::Attach { target: SessionTarget::Id(session_id), last_seen_offset: None })
                                .await;
                        }
                        writer = Some(new_writer);
                        daemon_rx = Some(new_rx);
                    }
                    ConnEvent::PairingRequired { code } => {
                        app.connection = ConnectionState::PairingRequired { code };
                    }
                    ConnEvent::Failed { text, attempt } => {
                        app.connection = ConnectionState::Reconnecting { class_text: text, attempt };
                    }
                }
            }

            // Inbound frame on the dedicated PTY binary channel, once open.
            pty_msg = async {
                match pty_in.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match pty_msg {
                    Some(Ok(WsMessage::Binary(bytes))) => {
                        if let Some((frame, _consumed)) = aihub_core::PtyBinaryFrame::decode(&bytes) {
                            app.vt_parser.process(&frame.data);
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => {
                        pty_out = None;
                        pty_in = None;
                    }
                }
            }

            // The dedicated PTY channel finished connecting.
            Some(PtyEvent::Ready { tx, rx }) = pty_rx_events.recv() => {
                pty_out = Some(tx);
                pty_in = Some(rx);
            }
        }
    }

    // Clean exit: restore terminal
    term_guard.restore();
    Ok(())
}

/// Handles `aihub doctor [--remote]` and returns without ever touching the terminal.
async fn run_doctor_command(cli: &Cli, remote_flag: bool) -> Result<()> {
    if !remote_flag {
        println!("aihub doctor: apenas --remote é suportado nesta versão.");
        return Ok(());
    }
    let url = match cli.daemon_target() {
        Some(raw) => match remote::parse_daemon_target(&raw) {
            RemoteTarget::Remote(u) => u,
            RemoteTarget::Local(_) => {
                println!("aihub doctor --remote requer um --daemon <URL> remoto (wss://...).");
                return Ok(());
            }
        },
        None => {
            println!("aihub doctor --remote requer --daemon <URL> (ou AIHUB_DAEMON).");
            return Ok(());
        }
    };
    let report = doctor::run_remote_doctor(&url, &default_identity_path()).await?;
    println!("{}", report.render());
    Ok(())
}

/// Drives one connection attempt against `target`, retrying forever with
/// backoff until it succeeds. The `sleep` between attempts runs in this
/// spawned task, never on the caller's draw loop (design doc §2.5).
async fn reconnect_task(
    target: RemoteTarget,
    autostart: bool,
    identity_path: PathBuf,
    events: mpsc::UnboundedSender<ConnEvent>,
) {
    let mut backoff = Backoff::new();
    loop {
        match &target {
            RemoteTarget::Local(socket_path) => match connect_local(socket_path, autostart).await {
                Ok(stream) => {
                    let (mut reader, mut writer) = stream.into_split();
                    if perform_handshake(&mut writer, &mut reader).await.is_ok() {
                        let daemon_rx = spawn_daemon_reader(reader);
                        let _ = events.send(ConnEvent::Connected {
                            writer: DaemonWriter::Uds(writer),
                            daemon_rx,
                        });
                        return;
                    }
                    let _ = events.send(ConnEvent::Failed {
                        text: "Conexão perdida. Tentando reconectar...".to_string(),
                        attempt: backoff.attempt() + 1,
                    });
                }
                Err(_) => {
                    let _ = events.send(ConnEvent::Failed {
                        text: "Conexão perdida. Tentando reconectar...".to_string(),
                        attempt: backoff.attempt() + 1,
                    });
                }
            },
            RemoteTarget::Remote(url) => {
                let identity = match Identity::load_or_create(&identity_path) {
                    Ok(id) => id,
                    Err(e) => {
                        let _ = events.send(ConnEvent::Failed {
                            text: format!("Falha ao carregar identidade local: {e}"),
                            attempt: backoff.attempt() + 1,
                        });
                        tokio::time::sleep(backoff.next_delay()).await;
                        continue;
                    }
                };
                match remote::connect(url, CONNECT_TIMEOUT).await {
                    Ok(socket) => {
                        let mut io = remote::spawn_io(socket);
                        match remote::perform_remote_handshake(&mut io, &identity, CONNECT_TIMEOUT)
                            .await
                        {
                            Ok(()) => {
                                let daemon_rx = remote::spawn_daemon_reader(io.inbound);
                                let _ = events.send(ConnEvent::Connected {
                                    writer: DaemonWriter::Ws(io.outbound),
                                    daemon_rx,
                                });
                                return;
                            }
                            Err(e) if e.class == FailureClass::Unauthorized => {
                                let _ = events.send(ConnEvent::PairingRequired {
                                    code: identity.pairing_code(),
                                });
                            }
                            Err(e) => {
                                let _ = events.send(ConnEvent::Failed {
                                    text: e.class.banner_text().to_string(),
                                    attempt: backoff.attempt() + 1,
                                });
                            }
                        }
                    }
                    Err(e) => {
                        let _ = events.send(ConnEvent::Failed {
                            text: e.class.banner_text().to_string(),
                            attempt: backoff.attempt() + 1,
                        });
                    }
                }
            }
        }
        tokio::time::sleep(backoff.next_delay()).await;
    }
}

/// Opens the dedicated PTY channel in the background so it never blocks the draw loop.
fn spawn_pty_connect(
    control_url: String,
    session_id: SessionId,
    ticket: ChannelTicket,
    events: mpsc::UnboundedSender<PtyEvent>,
) {
    tokio::spawn(async move {
        let pty_url = remote::pty_channel_url(&control_url);
        if let Ok(io) =
            remote::connect_pty_channel(&pty_url, &session_id, ticket, CONNECT_TIMEOUT).await
        {
            let _ = events.send(PtyEvent::Ready {
                tx: io.outbound,
                rx: io.inbound,
            });
        }
    });
}

/// Performs the initial attach-or-create bootstrap once the first connection
/// succeeds: reattaches to the latest session for the repo (or a named
/// session for `aihub attach <id>`), creating a new session if none exists.
async fn bootstrap_session(
    writer: &mut DaemonWriter,
    daemon_rx: &mut mpsc::UnboundedReceiver<Result<DaemonMessage>>,
    app: &mut App,
    cli: &Cli,
    repo_path: &Path,
) -> Result<()> {
    match &cli.command {
        Some(Commands::Attach { session_id }) => {
            let target = match session_id {
                Some(id) => SessionTarget::Id(SessionId::new(id.clone())),
                None => SessionTarget::LatestForRepo(repo_path.to_path_buf()),
            };
            writer
                .send(&ClientMessage::Attach {
                    target,
                    last_seen_offset: None,
                })
                .await?;
            if let Ok(Some(Ok(msg))) =
                tokio::time::timeout(Duration::from_secs(3), daemon_rx.recv()).await
            {
                handle_daemon_msg(app, msg);
            }
        }
        Some(Commands::Doctor { .. }) => unreachable!("doctor is handled before bootstrap"),
        None => {
            writer
                .send(&ClientMessage::Attach {
                    target: SessionTarget::LatestForRepo(repo_path.to_path_buf()),
                    last_seen_offset: None,
                })
                .await?;

            match tokio::time::timeout(Duration::from_secs(3), daemon_rx.recv()).await {
                Ok(Some(Ok(DaemonMessage::Attached {
                    session_id,
                    scrollback,
                    summary,
                    channel_ticket,
                    ..
                }))) => {
                    apply_attached(app, session_id, scrollback, summary, channel_ticket);
                }
                Ok(Some(Ok(DaemonMessage::QuotaPush { snapshots }))) => {
                    app.snapshots = snapshots;
                    if let Ok(Some(Ok(next))) =
                        tokio::time::timeout(Duration::from_secs(2), daemon_rx.recv()).await
                    {
                        match next {
                            DaemonMessage::Attached {
                                session_id,
                                scrollback,
                                summary,
                                channel_ticket,
                                ..
                            } => {
                                apply_attached(
                                    app,
                                    session_id,
                                    scrollback,
                                    summary,
                                    channel_ticket,
                                );
                            }
                            DaemonMessage::Error { .. } => {
                                create_new_session(
                                    writer,
                                    daemon_rx,
                                    app,
                                    repo_path,
                                    cli.task.clone(),
                                )
                                .await?;
                            }
                            other => handle_daemon_msg(app, other),
                        }
                    }
                }
                // Attach missed (no session yet) or was refused (e.g. LatestForRepo
                // pointed at another principal's session). Same recovery as the
                // QuotaPush+Error branch below: mint a fresh session so a remote
                // client is not left connected with an empty TUI and no PTY.
                Ok(Some(Ok(DaemonMessage::Error { .. }))) => {
                    create_new_session(
                        writer,
                        daemon_rx,
                        app,
                        repo_path,
                        cli.task.clone(),
                    )
                    .await?;
                }
                Ok(Some(Ok(other))) => {
                    handle_daemon_msg(app, other);
                }
                Ok(Some(Err(e))) => return Err(e),
                Ok(None) | Err(_) => {
                    create_new_session(writer, daemon_rx, app, repo_path, cli.task.clone()).await?;
                }
            }
        }
    }

    let _ = writer.send(&ClientMessage::RequestQuota).await;

    if let Some(task_text) = &cli.task {
        app.task = Some(task_text.clone());
        if let Some(session_id) = &app.session_id {
            let _ = writer
                .send(&ClientMessage::SubmitTask {
                    session_id: session_id.clone(),
                    task: task_text.clone(),
                })
                .await;
        }
    }

    Ok(())
}

fn apply_attached(
    app: &mut App,
    session_id: SessionId,
    scrollback: aihub_core::Base64Bytes,
    summary: SessionSummary,
    channel_ticket: Option<ChannelTicket>,
) {
    app.session_id = Some(session_id);
    app.harness = summary.harness;
    app.branch = summary.branch;
    app.mode = summary.mode;
    app.active = summary.active;
    app.worktree_path = summary.worktree_path;
    app.repo_path = summary.repo_path;
    app.vt_parser.process(scrollback.as_slice());
    if channel_ticket.is_some() {
        app.channel_ticket = channel_ticket;
    }
}

async fn create_new_session(
    writer: &mut DaemonWriter,
    daemon_rx: &mut mpsc::UnboundedReceiver<Result<DaemonMessage>>,
    app: &mut App,
    repo_path: &Path,
    initial_prompt: Option<String>,
) -> Result<()> {
    app.task = initial_prompt.clone();
    writer
        .send(&ClientMessage::NewSession {
            harness: HarnessId::ClaudeCode,
            repo_path: repo_path.to_path_buf(),
            initial_prompt,
        })
        .await?;

    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        match tokio::time::timeout(Duration::from_secs(3), daemon_rx.recv()).await {
            Ok(Some(Ok(DaemonMessage::SessionCreated {
                session_id,
                harness,
                worktree_path,
                branch,
                channel_ticket,
                ..
            }))) => {
                app.session_id = Some(session_id.clone());
                app.harness = harness;
                app.worktree_path = worktree_path;
                app.branch = branch;
                if channel_ticket.is_some() {
                    app.channel_ticket = channel_ticket;
                }

                writer
                    .send(&ClientMessage::Attach {
                        target: SessionTarget::Id(session_id),
                        last_seen_offset: None,
                    })
                    .await?;
                break;
            }
            Ok(Some(Ok(DaemonMessage::Attached {
                session_id,
                scrollback,
                summary,
                channel_ticket,
                ..
            }))) => {
                apply_attached(app, session_id, scrollback, summary, channel_ticket);
                break;
            }
            Ok(Some(Ok(other))) => {
                handle_daemon_msg(app, other);
            }
            Ok(Some(Err(e))) => return Err(e),
            Ok(None) | Err(_) => break,
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
            channel_ticket,
            ..
        } => {
            apply_attached(app, session_id, scrollback, summary, channel_ticket);
        }
        DaemonMessage::SessionCreated { channel_ticket, .. } => {
            if channel_ticket.is_some() {
                app.channel_ticket = channel_ticket;
            }
        }
        DaemonMessage::Detached { session_id } => {
            if let Some(curr) = &app.session_id {
                if curr != &session_id {
                    return;
                }
            }
            app.should_exit = true;
        }
        DaemonMessage::PtyOutput {
            session_id, data, ..
        } => {
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
            recommendation_id,
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
                        recommendation_id,
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
        DaemonMessage::Unauthorized => {
            app.set_status("Não autorizado");
        }
        DaemonMessage::SessionList { .. }
        | DaemonMessage::Hello { .. }
        | DaemonMessage::Challenge { .. } => {}
    }
}
