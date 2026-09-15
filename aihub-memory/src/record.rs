use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use aihub_core::{paths::default_data_dir, HarnessId, SessionId};
use serde::Serialize;

use crate::{BriefPair, MemoryError};

/// Append-only handoff log when the ai-memory daemon is not available locally.
/// Target schema mirrors `NewHandoff` in ai-memory (`crates/ai-memory-core/src/handoff.rs`, commit 968dac852aa4a41408ba4b9dcb2fe69eac957d21).
#[derive(Serialize)]
struct HandoffRecord<'a> {
    session_id: &'a str,
    from_harness: &'a str,
    to_harness: &'a str,
    brief_path: String,
    prompt_path: String,
    recorded_at: String,
}

pub async fn record_handoff(
    session_id: &SessionId,
    from: HarnessId,
    to: HarnessId,
    brief: &BriefPair,
) -> Result<(), MemoryError> {
    let data_dir = default_data_dir();
    std::fs::create_dir_all(&data_dir)?;
    let path = handoffs_log_path(&data_dir);
    let record = HandoffRecord {
        session_id: session_id.as_str(),
        from_harness: harness_label(from),
        to_harness: harness_label(to),
        brief_path: brief.brief_path.display().to_string(),
        prompt_path: brief.prompt_path.display().to_string(),
        recorded_at: time_now_rfc3339(),
    };
    append_jsonl(&path, &record)?;
    Ok(())
}

/// Records handoff metadata and returns whether it reached ai-memory or was spooled locally (§3.5).
///
/// Owned by Session 06.
pub async fn record_handoff_destination(
    session_id: &SessionId,
    from: HarnessId,
    to: HarnessId,
    brief: &BriefPair,
) -> Result<crate::HandoffDestination, MemoryError> {
    crate::ai_memory::record_handoff_destination(session_id, from, to, brief).await
}

/// Convenience alias returning true if delivered to ai-memory, false if spooled locally.
pub async fn record_handoff_delivered(
    session_id: &SessionId,
    from: HarnessId,
    to: HarnessId,
    brief: &BriefPair,
) -> Result<bool, MemoryError> {
    crate::ai_memory::record_handoff_delivered(session_id, from, to, brief).await
}

pub fn handoffs_log_path(data_dir: &Path) -> PathBuf {
    data_dir.join("handoffs.jsonl")
}

fn harness_label(h: HarnessId) -> &'static str {
    match h {
        HarnessId::ClaudeCode => "claude-code",
        HarnessId::Codex => "codex",
        HarnessId::CursorAgent => "cursor-agent",
        HarnessId::Antigravity => "antigravity",
    }
}

fn append_jsonl(path: &Path, value: &impl Serialize) -> Result<(), MemoryError> {
    let line = serde_json::to_string(value)?;
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{line}")?;
    Ok(())
}

fn time_now_rfc3339() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}Z", dur.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[tokio::test]
    async fn appends_jsonl_in_temp_data_dir() {
        let dir = std::env::temp_dir().join("aihub-memory-record-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let log = handoffs_log_path(&dir);
        let brief = BriefPair {
            brief_path: dir.join("01.md"),
            prompt_path: dir.join("01.prompt.md"),
        };
        let record = HandoffRecord {
            session_id: "sess-a",
            from_harness: "claude-code",
            to_harness: "codex",
            brief_path: brief.brief_path.display().to_string(),
            prompt_path: brief.prompt_path.display().to_string(),
            recorded_at: "0Z".into(),
        };
        append_jsonl(&log, &record).unwrap();
        let text = fs::read_to_string(&log).unwrap();
        assert!(text.contains("sess-a"));
        assert!(text.contains("claude-code"));
        let _ = fs::remove_dir_all(&dir);
    }
}
