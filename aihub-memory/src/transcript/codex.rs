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
    let mut file_session = String::new();
    let mut file_cwd = String::new();
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
        if value.get("type").and_then(Value::as_str) == Some("session_meta") {
            if let Some(payload) = value.get("payload") {
                if let Some(id) = payload.get("session_id").and_then(Value::as_str) {
                    file_session = id.to_string();
                }
                if let Some(cwd) = payload.get("cwd").and_then(Value::as_str) {
                    file_cwd = cwd.to_string();
                }
            }
            continue;
        }

        let session_ok = file_session == session_id;
        let cwd_ok = paths_match_worktree(&file_cwd, worktree_path);
        if !session_ok && !cwd_ok {
            continue;
        }

        if value.get("type").and_then(Value::as_str) != Some("response_item") {
            continue;
        }
        let Some(payload) = value.get("payload") else {
            continue;
        };
        let Some(role) = payload.get("role").and_then(Value::as_str) else {
            continue;
        };
        let Some(content) = payload.get("content") else {
            continue;
        };
        let Some(text) = codex_text(content) else {
            continue;
        };
        match role {
            "user" => last_user = redact_secrets(&text),
            "assistant" => {
                let redacted = redact_secrets(&text);
                decisions.extend(collect_decisions(&redacted));
                last_assistant = redacted;
            }
            _ => {}
        }
    }

    if last_assistant.is_empty() {
        return None;
    }

    let summary = if last_user.is_empty() {
        "Last harness turn captured from Codex transcript.".to_string()
    } else {
        truncate_chars(&last_user, 240)
    };

    Some(HandoffTurn {
        summary: redact_secrets(&summary),
        last_output: redact_secrets(&last_assistant),
        decisions: decisions.into_iter().map(|d| redact_secrets(&d)).collect(),
    })
}

fn codex_text(content: &Value) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(arr) = content.as_array() {
        for block in arr {
            if block.get("type").and_then(Value::as_str) == Some("output_text")
                || block.get("type").and_then(Value::as_str) == Some("input_text")
            {
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    parts.push(t.to_string());
                }
            }
        }
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
    fn extracts_codex_assistant_turn() {
        let dir = std::env::temp_dir().join("aihub-memory-codex-fixture");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rollout.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            r#"{{"type":"session_meta","payload":{{"session_id":"codex-9","cwd":"/tmp/wt"}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"response_item","payload":{{"role":"user","content":[{{"type":"input_text","text":"ship memory"}}]}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"response_item","payload":{{"role":"assistant","content":[{{"type":"output_text","text":"Decision: append JSONL records.\nBridge is ready."}}]}}}}"#
        )
        .unwrap();

        let turn = extract_file("codex-9", Path::new("/tmp/wt"), &path).unwrap();
        assert!(turn.last_output.contains("Bridge is ready"));
        assert!(turn.summary.contains("ship memory"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn f11_tool_call_skipped_for_final_assistant_message() {
        let dir = std::env::temp_dir().join("aihub-memory-codex-f11");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rollout_with_tool_call.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            r#"{{"type":"session_meta","payload":{{"session_id":"codex-f11","cwd":"/tmp/wt"}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"response_item","payload":{{"role":"user","content":[{{"type":"input_text","text":"check file"}}]}}}}"#
        )
        .unwrap();
        // Tool-call / function_call response_item with no role/content
        writeln!(
            f,
            r#"{{"type":"response_item","payload":{{"type":"function_call","name":"view_file","arguments":{{"path":"lib.rs"}}}}}}"#
        )
        .unwrap();
        // Followed by final assistant message
        writeln!(
            f,
            r#"{{"type":"response_item","payload":{{"role":"assistant","content":[{{"type":"output_text","text":"Decision: tool call handled.\nHere is the final answer."}}]}}}}"#
        )
        .unwrap();

        let turn = extract_file("codex-f11", Path::new("/tmp/wt"), &path)
            .expect("must not abort on tool call");
        assert!(turn.last_output.contains("Here is the final answer"));
        assert!(turn
            .decisions
            .iter()
            .any(|d| d.contains("tool call handled")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
