//! Application state models for the `aihub` TUI client.

use std::path::PathBuf;
use std::time::Instant;
use aihub_core::{
    HarnessId, MergeStrategy, Mode, QuotaSnapshot, SessionId, TaskTier,
};

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
    QuotaTable {
        scroll: usize,
    },
    /// Merge confirmation and diff review modal.
    MergeReview {
        diff: String,
        message: String,
        strategy: MergeStrategy,
        scroll: usize,
    },
}

/// Route recommendation from the daemon.
#[derive(Debug, Clone, PartialEq)]
pub struct RecommendationState {
    pub tier: TaskTier,
    pub harness: HarnessId,
    pub lane: Option<String>,
    pub holds_until_s: Option<u64>,
    pub confidence: f32,
    pub reason: String,
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
        }
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
