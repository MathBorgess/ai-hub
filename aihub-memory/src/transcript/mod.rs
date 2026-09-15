mod claude;
mod codex;
mod cursor;
mod paths;

use std::path::{Path, PathBuf};

use aihub_core::HarnessId;

pub(crate) use paths::discover_roots;

pub(crate) fn extract_from_roots(
    harness: HarnessId,
    session_id: &str,
    worktree_path: &Path,
    roots: &[PathBuf],
) -> Option<super::HandoffTurn> {
    match harness {
        HarnessId::ClaudeCode => claude::extract_from_roots(session_id, worktree_path, roots),
        HarnessId::Codex => codex::extract_from_roots(session_id, worktree_path, roots),
        HarnessId::CursorAgent => cursor::extract_from_roots(session_id, worktree_path, roots),
        HarnessId::Antigravity => None,
    }
}

pub fn no_turn_message() -> super::HandoffTurn {
    super::HandoffTurn {
        summary: "no last turn".to_string(),
        last_output: String::new(),
        decisions: Vec::new(),
    }
}
