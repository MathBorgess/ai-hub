use std::path::{Path, PathBuf};

use aihub_core::HarnessId;

pub fn discover_roots(harness: HarnessId) -> Vec<PathBuf> {
    let home = dirs_fallback();
    match harness {
        HarnessId::ClaudeCode => claude_config_dirs(&home)
            .into_iter()
            .map(|base| base.join("projects"))
            .filter(|p| p.is_dir())
            .collect(),
        HarnessId::Codex => codex_homes(&home)
            .into_iter()
            .flat_map(|base| {
                ["sessions", "archived_sessions"]
                    .into_iter()
                    .map(move |sub| base.join(sub))
            })
            .filter(|p| p.is_dir())
            .collect(),
        HarnessId::CursorAgent => {
            let projects = home.join(".cursor").join("projects");
            if projects.is_dir() {
                vec![projects]
            } else {
                Vec::new()
            }
        }
        HarnessId::Antigravity => Vec::new(),
    }
}

fn dirs_fallback() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn claude_config_dirs(home: &Path) -> Vec<PathBuf> {
    if let Ok(from_env) = std::env::var("CLAUDE_CONFIG_DIR") {
        let dirs: Vec<PathBuf> = from_env
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect();
        if !dirs.is_empty() {
            return dirs;
        }
    }
    vec![home.join(".config").join("claude"), home.join(".claude")]
}

fn codex_homes(home: &Path) -> Vec<PathBuf> {
    if let Ok(from_env) = std::env::var("CODEX_HOME") {
        let dirs: Vec<PathBuf> = from_env
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect();
        if !dirs.is_empty() {
            return dirs;
        }
    }
    vec![home.join(".codex")]
}

pub(crate) fn walk_jsonl_files(root: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > 8 || !root.is_dir() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_type().map(|t| t.is_symlink()).unwrap_or(false) {
            continue;
        }
        if path.is_dir() {
            walk_jsonl_files(&path, out, depth + 1);
        } else if path.extension().is_some_and(|e| e == "jsonl") {
            out.push(path);
        }
    }
}

pub(crate) fn paths_match_worktree(recorded: &str, worktree: &Path) -> bool {
    let Ok(canonical) = worktree.canonicalize() else {
        return recorded.contains(worktree.to_string_lossy().as_ref());
    };
    let recorded_path = PathBuf::from(recorded);
    if recorded_path == canonical {
        return true;
    }
    canonical
        .to_string_lossy()
        .contains(recorded.trim_end_matches('/'))
        || recorded.contains(canonical.to_string_lossy().as_ref())
}

pub(crate) fn collect_decisions(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let lower = trimmed.to_ascii_lowercase();
        if lower.starts_with("decision:")
            || lower.starts_with("- decision:")
            || lower.contains("we decided")
            || lower.starts_with("approach:")
        {
            out.push(trimmed.to_string());
        }
    }
    out
}

pub(crate) fn read_jsonl(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}
