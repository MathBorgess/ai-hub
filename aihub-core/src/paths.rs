use crate::types::SessionId;
use std::path::{Path, PathBuf};

/// Returns the default data directory for aihub (`~/.local/share/aihub`).
pub fn default_data_dir() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".local/share/aihub")
    } else {
        PathBuf::from("/tmp/aihub")
    }
}

/// Returns the default socket path (`~/.local/share/aihub/aihub.sock`).
pub fn default_socket_path() -> PathBuf {
    socket_path_in(&default_data_dir())
}

/// Returns the socket path within a custom data directory.
pub fn socket_path_in(data_dir: &Path) -> PathBuf {
    data_dir.join("aihub.sock")
}

/// Returns the default root directory for git worktrees.
///
/// On Linux this is `~/.local/share/aihub/worktrees`: the box Ailla target runs headless
/// with no desktop session to keep `/tmp` alive, and worktrees on a tmpfs-backed temp dir
/// would not survive a daemon restart (ADR §2.5, gap §4.4). On macOS the existing
/// `${TMPDIR}/aihub/worktrees` behavior is unchanged.
pub fn default_worktree_root() -> PathBuf {
    if cfg!(target_os = "linux") {
        linux_worktree_root()
    } else {
        let base = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
        worktree_root_in(Path::new(&base))
    }
}

/// Linux worktree root logic, split out so it is exercised by tests on every platform.
fn linux_worktree_root() -> PathBuf {
    default_data_dir().join("worktrees")
}

/// Returns the root directory for git worktrees within a custom base temporary directory.
pub fn worktree_root_in(tmp_dir: &Path) -> PathBuf {
    tmp_dir.join("aihub/worktrees")
}

/// Returns the worktree directory for a specific session within a worktree root.
pub fn session_worktree_dir(worktree_root: &Path, session_id: &SessionId) -> PathBuf {
    worktree_root.join(session_id.as_str())
}

/// Returns the standard git branch name for a session (`session/<session-id>`).
pub fn session_branch_name(session_id: &SessionId) -> String {
    format!("session/{}", session_id.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_custom_paths() {
        let temp = Path::new("/custom/tmp");
        let data = Path::new("/custom/data");
        assert_eq!(
            socket_path_in(data),
            PathBuf::from("/custom/data/aihub.sock")
        );
        let root = worktree_root_in(temp);
        assert_eq!(root, PathBuf::from("/custom/tmp/aihub/worktrees"));

        let sid = SessionId::new("test-sess-123");
        assert_eq!(
            session_worktree_dir(&root, &sid),
            PathBuf::from("/custom/tmp/aihub/worktrees/test-sess-123")
        );
        assert_eq!(session_branch_name(&sid), "session/test-sess-123");
    }

    #[test]
    fn test_linux_worktree_root_uses_persistent_data_dir() {
        // ADR §2.5/§4.4: box Ailla worktrees must not live on tmpfs so they survive
        // a daemon restart. Exercised directly since it must hold on every CI runner,
        // not only when tests execute on Linux.
        assert_eq!(linux_worktree_root(), default_data_dir().join("worktrees"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_default_worktree_root_on_linux() {
        assert_eq!(default_worktree_root(), linux_worktree_root());
    }
}
