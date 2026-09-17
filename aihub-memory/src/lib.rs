mod ai_memory;
mod brief;
mod record;
mod redact;
mod transcript;

use std::path::{Path, PathBuf};

use aihub_core::{HarnessId, SessionId};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use ai_memory::{drain_spool, drain_spool_with_hook, record_handoff_to, SpooledRecord};
pub use brief::{next_session_index, write_brief_pair};
pub use record::{
    drain_spooled_handoffs, drain_spooled_handoffs_to, handoffs_log_path, record_handoff,
    record_handoff_delivered, record_handoff_destination,
};

/// Reports whether the recorded handoff reached the live backend or was spooled locally (§3.5).
/// Backend-neutral by design: the concrete backend (currently ai-memory) is an
/// implementation detail confined to `ai_memory.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffDestination {
    Delivered,
    Spooled,
}

impl HandoffDestination {
    pub fn is_delivered(&self) -> bool {
        matches!(self, HandoffDestination::Delivered)
    }

    pub fn is_spooled(&self) -> bool {
        matches!(self, HandoffDestination::Spooled)
    }
}

#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("Failed to extract harness transcript or history: {0}")]
    ExtractionFailed(String),

    #[error("Memory serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Spool storage limit reached: {0}")]
    SpoolFull(String),

    #[error("Spool lock error: {0}")]
    LockError(String),

    #[error("Unsafe spool file or directory permissions: {0}")]
    UnsafePermissions(String),

    #[error("Handoff delivery failed: {0}")]
    DeliveryFailed(String),
}

/// Extracted context and decisions from the outgoing harness's final turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffTurn {
    pub summary: String,
    pub last_output: String,
    pub decisions: Vec<String>,
}

/// File paths to the generated brief (`NN.md`) and launch prompt (`NN.prompt.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BriefPair {
    pub brief_path: PathBuf,
    pub prompt_path: PathBuf,
}

/// Extracts the last turn and decisions of the outgoing harness from worktree logs / transcripts.
pub async fn extract_last_turn(
    session_id: &SessionId,
    harness: HarnessId,
    worktree_path: &Path,
) -> Result<HandoffTurn, MemoryError> {
    let session_id = session_id.as_str().to_string();
    let worktree = worktree_path.to_path_buf();
    let harness_copy = harness;

    tokio::task::spawn_blocking(move || {
        let roots = transcript::discover_roots(harness_copy);
        if roots.is_empty() {
            return Ok(transcript::no_turn_message());
        }
        let turn = transcript::extract_from_roots(harness_copy, &session_id, &worktree, &roots);
        Ok(turn.unwrap_or_else(transcript::no_turn_message))
    })
    .await
    .map_err(|e| MemoryError::ExtractionFailed(e.to_string()))?
}

/// Reports whether local transcript roots exist for a harness (for diagnostics/tests).
pub fn transcript_roots_for(harness: HarnessId) -> Vec<PathBuf> {
    transcript::discover_roots(harness)
}

#[cfg(test)]
mod integration {
    use super::*;
    use std::fs;
    use std::io::Write;

    #[test]
    fn agy_roots_under_antigravity_brain() {
        let roots = transcript_roots_for(HarnessId::Antigravity);
        assert!(roots.iter().all(|p| p.ends_with("brain")));
    }

    #[test]
    fn cursor_roots_under_cursor_projects() {
        let roots = transcript_roots_for(HarnessId::CursorAgent);
        assert!(roots.iter().all(|p| p.ends_with("projects")));
    }

    #[tokio::test]
    async fn extract_uses_fixture_roots_via_internal_modules() {
        let dir = std::env::temp_dir().join(format!(
            "aihub-memory-integration-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        let mut f = fs::File::create(&path).unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","sessionId":"abc","cwd":"/wt","message":{{"content":[{{"type":"text","text":"final answer"}}]}}}}"#
        )
        .unwrap();

        let turn = transcript::extract_from_roots(
            HarnessId::ClaudeCode,
            "abc",
            Path::new("/wt"),
            std::slice::from_ref(&dir),
        )
        .unwrap();
        assert_eq!(turn.last_output, "final answer");
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn extract_antigravity_turn_from_fixture() {
        let dir = std::env::temp_dir().join(format!(
            "aihub-memory-agy-integration-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        let logs = dir.join("sess-test").join(".system_generated").join("logs");
        fs::create_dir_all(&logs).unwrap();
        let path = logs.join("transcript.jsonl");
        let mut f = fs::File::create(&path).unwrap();
        writeln!(
            f,
            r#"{{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-09-15T18:00:00Z","content":"<USER_REQUEST>\nwork in /wt\n</USER_REQUEST>"}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-09-15T18:00:05Z","content":"Decision: complete task.\nFinished working on /wt."}}"#
        )
        .unwrap();

        let turn = transcript::extract_from_roots(
            HarnessId::Antigravity,
            "sess-test",
            Path::new("/wt"),
            std::slice::from_ref(&dir),
        )
        .unwrap();
        assert!(turn.last_output.contains("Finished working"));
        assert!(turn.decisions.iter().any(|d| d.contains("complete task")));
        let _ = fs::remove_dir_all(&dir);
    }
}
