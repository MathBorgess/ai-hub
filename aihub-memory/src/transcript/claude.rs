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
        if let Some(turn) = extract_file(session_id, worktree_path, &path) {
            return Some(turn);
        }
    }
    None
}

fn extract_file(session_id: &str, worktree_path: &Path, path: &Path) -> Option<HandoffTurn> {
    let text = read_jsonl(path)?;
    let mut last_user = String::new();
    let mut last_assistant = String::new();
    let mut decisions = Vec::new();
    let mut matched = false;

    for line in text.lines() {
        if line.len() < 2 {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("isApiErrorMessage").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let line_session = value
            .get("sessionId")
            .or_else(|| value.get("session_id"))
            .and_then(Value::as_str);
        let cwd = value.get("cwd").and_then(Value::as_str);
        let session_ok = line_session == Some(session_id);
        let cwd_ok = cwd.is_some_and(|c| paths_match_worktree(c, worktree_path));
        if !session_ok && !cwd_ok {
            continue;
        }
        matched = true;

        if value.get("type").and_then(Value::as_str) == Some("user") {
            if let Some(text) = claude_user_text(&value) {
                last_user = redact_secrets(&text);
            }
        }
        if value.get("type").and_then(Value::as_str) == Some("assistant") {
            if let Some(text) = claude_assistant_text(&value) {
                let redacted = redact_secrets(&text);
                decisions.extend(collect_decisions(&redacted));
                last_assistant = redacted;
            }
        }
    }

    if !matched || last_assistant.is_empty() {
        return None;
    }

    let summary = if last_user.is_empty() {
        "Last harness turn captured from Claude Code transcript.".to_string()
    } else {
        truncate_chars(&last_user, 240)
    };

    Some(HandoffTurn {
        summary: redact_secrets(&summary),
        last_output: redact_secrets(&last_assistant),
        decisions: decisions.into_iter().map(|d| redact_secrets(&d)).collect(),
    })
}

fn claude_user_text(value: &Value) -> Option<String> {
    message_text(value.get("message")?)
}

fn claude_assistant_text(value: &Value) -> Option<String> {
    message_text(value.get("message")?)
}

fn message_text(message: &Value) -> Option<String> {
    let content = message.get("content")?;
    let mut parts = Vec::new();
    if let Some(arr) = content.as_array() {
        for block in arr {
            if block.get("type").and_then(Value::as_str) == Some("text") {
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    parts.push(t.to_string());
                }
            }
        }
    } else if let Some(t) = content.as_str() {
        parts.push(t.to_string());
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
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
    fn extracts_last_assistant_from_fixture() {
        let dir = std::env::temp_dir().join("aihub-memory-claude-fixture");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sess.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            r#"{{"type":"user","sessionId":"sess-1","cwd":"/tmp/wt","message":{{"content":[{{"type":"text","text":"implement memory bridge"}}]}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","sessionId":"sess-1","cwd":"/tmp/wt","message":{{"content":[{{"type":"text","text":"Decision: use JSONL until ai-memory is wired.\nDone with the bridge."}}]}}}}"#
        )
        .unwrap();

        let turn = extract_file("sess-1", Path::new("/tmp/wt"), &path).unwrap();
        assert!(turn.last_output.contains("Done with the bridge"));
        assert!(turn.decisions.iter().any(|d| d.contains("JSONL")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
