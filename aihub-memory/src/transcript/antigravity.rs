use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::redact::redact_secrets;
use crate::HandoffTurn;

use super::paths::{collect_decisions, paths_match_worktree, read_jsonl, walk_jsonl_files};

pub(crate) fn extract_from_roots(
    session_id: &str,
    worktree_path: &Path,
    roots: &[PathBuf],
) -> Option<HandoffTurn> {
    let mut files = Vec::new();
    for root in roots {
        walk_jsonl_files(root, &mut files, 0);
    }
    files.sort_by_key(|p| {
        p.metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
    });
    files.reverse();

    for path in files {
        if path.file_name().and_then(|n| n.to_str()) != Some("transcript.jsonl") {
            continue;
        }
        if let Some(turn) = extract_file(session_id, worktree_path, &path) {
            return Some(turn);
        }
    }
    None
}

fn extract_file(session_id: &str, worktree_path: &Path, path: &Path) -> Option<HandoffTurn> {
    let text = read_jsonl(path)?;

    let path_str = path.to_string_lossy();
    let session_matches_path = path_str.contains(session_id);
    let mut matched = session_matches_path;

    let mut last_user = String::new();
    let mut last_assistant = String::new();
    let mut decisions = Vec::new();

    for line in text.lines() {
        if line.len() < 2 {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };

        if !matched {
            let content_str = value.get("content").and_then(Value::as_str).unwrap_or("");
            if content_str.contains(session_id) || paths_match_worktree(content_str, worktree_path)
            {
                matched = true;
            }
            if let Some(tool_calls) = value.get("tool_calls").and_then(Value::as_array) {
                for tc in tool_calls {
                    let tc_str = tc.to_string();
                    if tc_str.contains(session_id) || paths_match_worktree(&tc_str, worktree_path) {
                        matched = true;
                        break;
                    }
                }
            }
        }

        let source = value.get("source").and_then(Value::as_str).unwrap_or("");
        let msg_type = value.get("type").and_then(Value::as_str).unwrap_or("");

        if source == "USER_EXPLICIT" || msg_type == "USER_INPUT" {
            if let Some(content) = value.get("content").and_then(Value::as_str) {
                if !content.trim().is_empty() {
                    last_user = redact_secrets(content);
                }
            }
        } else if source == "MODEL" && msg_type == "PLANNER_RESPONSE" {
            if let Some(content) = value.get("content").and_then(Value::as_str) {
                if !content.trim().is_empty() {
                    let redacted = redact_secrets(content);
                    decisions.extend(collect_decisions(&redacted));
                    last_assistant = redacted;
                }
            }
        }
    }

    if !matched || last_assistant.is_empty() {
        return None;
    }

    let summary = if last_user.is_empty() {
        "Last harness turn captured from Antigravity transcript.".to_string()
    } else {
        truncate_chars(&last_user, 240)
    };

    Some(HandoffTurn {
        summary: redact_secrets(&summary),
        last_output: redact_secrets(&last_assistant),
        decisions: decisions.into_iter().map(|d| redact_secrets(&d)).collect(),
    })
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn extracts_antigravity_assistant_turn() {
        let dir =
            std::env::temp_dir().join(format!("aihub-memory-agy-fixture-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let logs_dir = dir
            .join("sess-agy-123")
            .join(".system_generated")
            .join("logs");
        std::fs::create_dir_all(&logs_dir).unwrap();
        let path = logs_dir.join("transcript.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();

        writeln!(
            f,
            r#"{{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-09-15T18:00:00Z","content":"Please build the memory adapter."}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-09-15T18:00:01Z","tool_calls":[{{"name":"view_file","args":{{"AbsolutePath":"/tmp/wt/src/lib.rs"}}}}]}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"step_index":2,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-09-15T18:00:05Z","content":"Decision: implement adapter in ai_memory.rs.\nThe adapter is ready."}}"#
        )
        .unwrap();

        let turn = extract_file("sess-agy-123", Path::new("/tmp/wt"), &path).unwrap();
        assert!(turn.last_output.contains("The adapter is ready"));
        assert!(turn.decisions.iter().any(|d| d.contains("ai_memory.rs")));
        assert!(turn.summary.contains("Please build the memory adapter"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
