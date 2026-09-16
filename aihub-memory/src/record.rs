use std::path::{Path, PathBuf};

use aihub_core::{paths::default_data_dir, HarnessId, SessionId};

use crate::ai_memory::{self, SpooledRecord};
use crate::{BriefPair, HandoffDestination, MemoryError};

/// Public API: Appends a handoff record to the local spool.
pub async fn record_handoff(
    session_id: &SessionId,
    from: HarnessId,
    to: HarnessId,
    brief: &BriefPair,
    project: &str,
) -> Result<(), MemoryError> {
    let data_dir = default_data_dir();
    let spool_path = handoffs_log_path(&data_dir);
    let record = SpooledRecord::from_brief(session_id, from, to, brief, project)?;
    ai_memory::append_to_spool_file(&spool_path, &record)?;
    Ok(())
}

/// Records handoff metadata and returns whether it reached the live backend or was spooled locally (§3.5).
pub async fn record_handoff_destination(
    session_id: &SessionId,
    from: HarnessId,
    to: HarnessId,
    brief: &BriefPair,
    project: &str,
) -> Result<HandoffDestination, MemoryError> {
    crate::ai_memory::record_handoff_destination(session_id, from, to, brief, project).await
}

/// Convenience alias returning true if delivered to the live backend, false if spooled locally.
pub async fn record_handoff_delivered(
    session_id: &SessionId,
    from: HarnessId,
    to: HarnessId,
    brief: &BriefPair,
    project: &str,
) -> Result<bool, MemoryError> {
    crate::ai_memory::record_handoff_delivered(session_id, from, to, brief, project).await
}

/// Public drain call that can be run on a timer without a new handoff (N5).
/// Uses environment variables `AI_MEMORY_SERVER_URL`, `AI_MEMORY_AUTH_TOKEN`, and `AIHUB_DATA_DIR`.
pub async fn drain_spooled_handoffs(limit: usize) -> Result<usize, MemoryError> {
    let server_url = std::env::var("AI_MEMORY_SERVER_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:49374".to_string());
    let auth_token = std::env::var("AI_MEMORY_AUTH_TOKEN").ok();
    let data_dir = std::env::var_os("AIHUB_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(default_data_dir);

    drain_spooled_handoffs_to(&server_url, auth_token.as_deref(), &data_dir, limit).await
}

/// Public drain call taking explicit server URL, auth token, and data directory (N5).
pub async fn drain_spooled_handoffs_to(
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    limit: usize,
) -> Result<usize, MemoryError> {
    let spool_path = handoffs_log_path(data_dir);
    crate::ai_memory::drain_spool(server_url, auth_token, &spool_path, limit).await
}

pub fn handoffs_log_path(data_dir: &Path) -> PathBuf {
    data_dir.join("handoffs.jsonl")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[tokio::test]
    async fn appends_jsonl_in_temp_data_dir() {
        let dir = std::env::temp_dir().join(format!(
            "aihub-memory-record-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let log = handoffs_log_path(&dir);
        let brief_path = dir.join("01.md");
        fs::write(&brief_path, "## Goal\ntest record\n").unwrap();
        let brief = BriefPair {
            brief_path,
            prompt_path: dir.join("01.prompt.md"),
        };
        let sid = SessionId::new("sess-a");
        let record = SpooledRecord::from_brief(
            &sid,
            HarnessId::ClaudeCode,
            HarnessId::Codex,
            &brief,
            "my-proj",
        )
        .unwrap();

        ai_memory::append_to_spool_file(&log, &record).unwrap();
        let text = fs::read_to_string(&log).unwrap();
        assert!(text.contains("sess-a"));
        assert!(text.contains("claude-code"));
        assert!(text.contains("my-proj"));
        assert!(text.contains("test record"));
        let _ = fs::remove_dir_all(&dir);
    }
}
