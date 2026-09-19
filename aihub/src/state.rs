//! Application state models for the `aihub` TUI client.

use aihub_core::{
    ChannelTicket, HarnessId, MergeStrategy, Mode, QuotaSnapshot, SessionId, TaskTier,
};
use std::path::PathBuf;
use std::time::Instant;

/// UI display mode / overlay state.
#[derive(Debug, Clone, PartialEq)]
pub enum UiMode {
    /// Normal interactive terminal view forwarding keystrokes to PTY.
    Normal,
    /// Command palette active with text input.
    Palette {
        input: String,
        selected_index: usize,
    },
    /// Full windows x lanes quota table modal.
    QuotaTable { scroll: usize },
    /// Merge confirmation and diff review modal.
    MergeReview {
        diff: String,
        message: String,
        strategy: MergeStrategy,
        scroll: usize,
    },
}

/// Connection lifecycle state driven by the async reconnect machine (design doc
/// §2.5), rendered as a persistent banner distinct from the transient
/// `status_message` footer line.
#[derive(Debug, Clone, PartialEq)]
pub enum ConnectionState {
    /// Establishing the first connection.
    Connecting,
    /// Connected and past the v3 handshake (or connected on the local socket,
    /// which never challenges).
    Connected,
    /// Remote daemon rejected the credential (ADR gap §4.5): not yet paired.
    PairingRequired { code: String },
    /// Lost connection; the background task is retrying with backoff.
    Reconnecting { class_text: String, attempt: u32 },
}

/// Route recommendation or no-capacity status from the daemon.
#[derive(Debug, Clone, PartialEq)]
pub enum RecommendationState {
    Recommended {
        tier: TaskTier,
        harness: HarnessId,
        lane: Option<String>,
        model: Option<String>,
        holds_until_s: Option<u64>,
        confidence: f32,
        reason: String,
        recommendation_id: u64,
    },
    NoCapacity {
        reason: String,
    },
}

/// Central state of the aihub TUI client.
pub struct App {
    pub session_id: Option<SessionId>,
    pub harness: HarnessId,
    pub mode: Mode,
    pub branch: String,
    pub repo_path: PathBuf,
    pub worktree_path: PathBuf,
    pub vt_parser: vt100::Parser,
    pub snapshots: Vec<QuotaSnapshot>,
    pub recommendation: Option<RecommendationState>,
    pub prefix_active: bool,
    pub ui_mode: UiMode,
    pub status_message: Option<(String, Instant)>,
    pub should_exit: bool,
    pub last_terminal_size: (u16, u16),
    pub active: bool,
    pub task: Option<String>,
    pub pending_autonomous: bool,
    pub now_override: Option<u64>,
    pub connection: ConnectionState,
    /// Ticket admitting the secondary PTY channel for the current session
    /// (ADR contradiction 1, Option B), captured off `SessionCreated`/`Attached`
    /// so the run loop can open the dedicated PTY WebSocket once available.
    pub channel_ticket: Option<ChannelTicket>,
}

impl App {
    /// Creates a new App initialized with defaults.
    pub fn new(repo_path: PathBuf) -> Self {
        Self {
            session_id: None,
            harness: HarnessId::ClaudeCode,
            mode: Mode::Assisted,
            branch: "main".to_string(),
            repo_path,
            worktree_path: PathBuf::new(),
            vt_parser: vt100::Parser::new(24, 80, 2000),
            snapshots: Vec::new(),
            recommendation: None,
            prefix_active: false,
            ui_mode: UiMode::Normal,
            status_message: None,
            should_exit: false,
            last_terminal_size: (80, 24),
            active: true,
            task: None,
            pending_autonomous: false,
            now_override: None,
            connection: ConnectionState::Connecting,
            channel_ticket: None,
        }
    }

    /// Checks if a non-empty task description is currently stored.
    pub fn has_task(&self) -> bool {
        self.task.as_ref().is_some_and(|t| !t.trim().is_empty())
    }

    /// Set a transient status message to be shown in the footer.
    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status_message = Some((msg.into(), Instant::now()));
    }

    /// Clear status message if older than duration.
    pub fn clear_expired_status(&mut self, max_age: std::time::Duration) {
        if let Some((_, timestamp)) = &self.status_message {
            if timestamp.elapsed() > max_age {
                self.status_message = None;
            }
        }
    }
}
