use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::HandoffTurn;
use crate::redact::redact_secrets;

use super::paths::{collect_decisions, read_jsonl, walk_jsonl_files};

pub(crate) fn extract_from_roots(
    _session_id: &str,
    worktree_path: &Path,
    roots: &[PathBuf],
) -> Option<HandoffTurn> {
    let mut files = Vec::new();
    for root in roots {
        walk_jsonl_files(root, &mut files, 0);
    }
    files.retain(|p| path_mentions_worktree(p, worktree_path));
    files.sort_by_key(|p| {
        p.metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
    });
    files.reverse();

    for path in files {
        if let Some(turn) = extract_file(&path) {
            return Some(turn);
        }
    }
    None
}

fn path_mentions_worktree(path: &Path, worktree: &Path) -> bool {
    let lossy = path.to_string_lossy();
    lossy.contains(&worktree_slug(worktree))
}

fn worktree_slug(worktree: &Path) -> String {
    worktree
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| worktree.to_string_lossy().replace('/', "-"))
}

fn extract_file(path: &Path) -> Option<HandoffTurn> {
    let text = read_jsonl(path)?;
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
        let role = value.get("role").and_then(Value::as_str)?;
        let content = value.get("message")?.get("content")?;
        let text = cursor_text(content)?;
        match role {
            "user" => last_user = text,
            "assistant" => {
                decisions.extend(collect_decisions(&text));
                last_assistant = text;
            }
            _ => {}
        }
    }

    if last_assistant.is_empty() {
        return None;
    }

    let summary = if last_user.is_empty() {
        "Last harness turn captured from Cursor agent transcript.".to_string()
    } else {
        truncate_chars(&last_user, 240)
    };

    Some(HandoffTurn {
        summary: redact_secrets(&summary),
        last_output: redact_secrets(&last_assistant),
        decisions: decisions
            .into_iter()
            .map(|d| redact_secrets(&d))
            .collect(),
    })
}

fn cursor_text(content: &Value) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(arr) = content.as_array() {
        for block in arr {
            if block.get("type").and_then(Value::as_str) == Some("text") {
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
