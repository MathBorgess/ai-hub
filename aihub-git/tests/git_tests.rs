use aihub_core::{MergeStrategy, SessionId};
use aihub_git::{create_session_worktree, diff, finish, GitError};
use std::path::{Path, PathBuf};
use std::process::Command;

use std::sync::atomic::{AtomicU64, Ordering};

static TEST_COUNTER: AtomicU64 = AtomicU64::new(1);

struct TestRepo {
    dir: PathBuf,
    worktree_root: PathBuf,
}

impl TestRepo {
    fn new() -> Self {
        let count = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let rand_suffix: u64 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let base = std::env::temp_dir().join(format!("aihub-git-test-{}-{}-{}", std::process::id(), rand_suffix, count));
        let repo_dir = base.join("repo");
        let wt_root = base.join("worktrees");
        std::fs::create_dir_all(&repo_dir).expect("failed to create temp test repo dir");
        std::fs::create_dir_all(&wt_root).expect("failed to create temp test wt root dir");

        let repo = Self {
            dir: repo_dir,
            worktree_root: wt_root,
        };
        repo.git(&["init", "-b", "main"]);
        repo.git(&["config", "user.name", "Test User"]);
        repo.git(&["config", "user.email", "test@example.com"]);

        // Initial commit
        std::fs::write(repo.dir.join("init.txt"), "initial commit\n").unwrap();
        repo.git(&["add", "init.txt"]);
        repo.git(&["commit", "-m", "initial commit"]);

        repo
    }

    fn path(&self) -> &Path {
        &self.dir
    }

    fn worktree_root(&self) -> &Path {
        &self.worktree_root
    }

    fn git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.dir)
            .output()
            .unwrap_or_else(|e| panic!("failed to execute git {args:?}: {e}"));
        assert!(
            output.status.success(),
            "git {:?} failed: stderr={}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }
}

impl Drop for TestRepo {
    fn drop(&mut self) {
        if let Some(parent) = self.dir.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }
}

#[tokio::test]
async fn test_create_session_worktree() {
    let repo = TestRepo::new();
    let root = repo.worktree_root();
    let session_id = SessionId::new("sess-test-01");

    let worktree = create_session_worktree(repo.path(), &session_id, Some(root))
        .await
        .expect("create_session_worktree should succeed");

    assert_eq!(worktree.session_id, session_id);
    assert_eq!(worktree.branch, "session/sess-test-01");
    assert_eq!(worktree.base_branch, "main");
    assert!(worktree.path.exists());
    assert_eq!(worktree.path, root.join("sess-test-01"));

    // Verify worktree has init.txt
    assert!(worktree.path.join("init.txt").exists());
}

#[tokio::test]
async fn test_diff_plain_and_colored() {
    let repo = TestRepo::new();
    let root = repo.worktree_root();
    let session_id = SessionId::new("sess-diff-01");

    let worktree = create_session_worktree(repo.path(), &session_id, Some(root))
        .await
        .expect("create_session_worktree should succeed");

    // Add uncommitted change
    std::fs::write(worktree.path.join("feature.txt"), "feature line\n").unwrap();

    let plain_diff = diff(&worktree.path, "main", false)
        .await
        .expect("plain diff should succeed");
    assert!(plain_diff.contains("diff --git a/feature.txt b/feature.txt"));
    assert!(plain_diff.contains("+feature line"));
    assert!(!plain_diff.contains("\x1b["));

    let colored_diff = diff(&worktree.path, "main", true)
        .await
        .expect("colored diff should succeed");
    assert!(colored_diff.contains("diff --git a/feature.txt b/feature.txt"));
    assert!(colored_diff.contains("\x1b["));
}

#[tokio::test]
async fn test_finish_squash() {
    let repo = TestRepo::new();
    let root = repo.worktree_root();
    let session_id = SessionId::new("sess-squash-01");

    let worktree = create_session_worktree(repo.path(), &session_id, Some(root))
        .await
        .expect("create_session_worktree should succeed");

    // Modify file and add new file in worktree
    std::fs::write(worktree.path.join("init.txt"), "modified in worktree\n").unwrap();
    std::fs::write(worktree.path.join("new_file.txt"), "new file\n").unwrap();

    let outcome = finish(&worktree.path, MergeStrategy::Squash, "main")
        .await
        .expect("squash finish should succeed");

    assert_eq!(outcome.strategy, MergeStrategy::Squash);
    assert!(outcome.success);
    assert!(outcome.diff.contains("modified in worktree"));

    // Verify main checkout
    assert!(!worktree.path.exists());
    let main_init = std::fs::read_to_string(repo.path().join("init.txt")).unwrap();
    assert_eq!(main_init, "modified in worktree\n");
    let main_new = std::fs::read_to_string(repo.path().join("new_file.txt")).unwrap();
    assert_eq!(main_new, "new file\n");

    // Branch session/sess-squash-01 should be deleted
    let branches = repo.git(&["branch"]);
    assert!(!branches.contains("session/sess-squash-01"));
}

#[tokio::test]
async fn test_finish_fast_forward() {
    let repo = TestRepo::new();
    let root = repo.worktree_root();
    let session_id = SessionId::new("sess-ff-01");

    let worktree = create_session_worktree(repo.path(), &session_id, Some(root))
        .await
        .expect("create_session_worktree should succeed");

    // Write file in worktree
    std::fs::write(worktree.path.join("ff.txt"), "fast forward content\n").unwrap();

    let outcome = finish(&worktree.path, MergeStrategy::FastForward, "main")
        .await
        .expect("fast-forward finish should succeed");

    assert_eq!(outcome.strategy, MergeStrategy::FastForward);
    assert!(outcome.success);

    assert!(!worktree.path.exists());
    let content = std::fs::read_to_string(repo.path().join("ff.txt")).unwrap();
    assert_eq!(content, "fast forward content\n");

    let branches = repo.git(&["branch"]);
    assert!(!branches.contains("session/sess-ff-01"));
}

#[tokio::test]
async fn test_finish_keep() {
    let repo = TestRepo::new();
    let root = repo.worktree_root();
    let session_id = SessionId::new("sess-keep-01");

    let worktree = create_session_worktree(repo.path(), &session_id, Some(root))
        .await
        .expect("create_session_worktree should succeed");

    std::fs::write(worktree.path.join("keep.txt"), "keep content\n").unwrap();

    let outcome = finish(&worktree.path, MergeStrategy::Keep, "main")
        .await
        .expect("keep finish should succeed");

    assert_eq!(outcome.strategy, MergeStrategy::Keep);
    assert!(outcome.success);
    assert!(!worktree.path.exists());

    // Main checkout should NOT have keep.txt
    assert!(!repo.path().join("keep.txt").exists());

    // Branch session/sess-keep-01 MUST still exist
    let branches = repo.git(&["branch"]);
    assert!(branches.contains("session/sess-keep-01"));
}

#[tokio::test]
async fn test_finish_discard() {
    let repo = TestRepo::new();
    let root = repo.worktree_root();
    let session_id = SessionId::new("sess-discard-01");

    let worktree = create_session_worktree(repo.path(), &session_id, Some(root))
        .await
        .expect("create_session_worktree should succeed");

    std::fs::write(worktree.path.join("discard.txt"), "discard content\n").unwrap();

    let outcome = finish(&worktree.path, MergeStrategy::Discard, "main")
        .await
        .expect("discard finish should succeed");

    assert_eq!(outcome.strategy, MergeStrategy::Discard);
    assert!(outcome.success);
    assert!(!worktree.path.exists());

    // Main repo should NOT have discard.txt
    assert!(!repo.path().join("discard.txt").exists());

    // Branch session/sess-discard-01 MUST NOT exist
    let branches = repo.git(&["branch"]);
    assert!(!branches.contains("session/sess-discard-01"));
}

#[tokio::test]
async fn test_refuse_when_dirty() {
    let repo = TestRepo::new();
    let root = repo.worktree_root();
    let session_id = SessionId::new("sess-dirty-01");

    let worktree = create_session_worktree(repo.path(), &session_id, Some(root))
        .await
        .expect("create_session_worktree should succeed");

    std::fs::write(worktree.path.join("wt.txt"), "from wt\n").unwrap();

    // Make main repo dirty
    std::fs::write(repo.path().join("dirty.txt"), "dirty uncommitted\n").unwrap();

    let result = finish(&worktree.path, MergeStrategy::Squash, "main").await;
    match result {
        Err(GitError::CommandFailed(msg)) => {
            assert!(msg.contains("uncommitted changes"));
        }
        other => panic!("expected GitError::CommandFailed, got {other:?}"),
    }

    // Ensure worktree was NOT removed
    assert!(worktree.path.exists());
}

#[tokio::test]
async fn test_refuse_when_branch_changed() {
    let repo = TestRepo::new();
    let root = repo.worktree_root();
    let session_id = SessionId::new("sess-branch-01");

    let worktree = create_session_worktree(repo.path(), &session_id, Some(root))
        .await
        .expect("create_session_worktree should succeed");

    std::fs::write(worktree.path.join("wt.txt"), "from wt\n").unwrap();

    // Switch main repo to a new branch 'other-branch'
    repo.git(&["checkout", "-b", "other-branch"]);

    let result = finish(&worktree.path, MergeStrategy::Squash, "main").await;
    match result {
        Err(GitError::CommandFailed(msg)) => {
            assert!(msg.contains("other-branch"));
        }
        other => panic!("expected GitError::CommandFailed, got {other:?}"),
    }

    // Ensure worktree was NOT removed
    assert!(worktree.path.exists());
}

#[tokio::test]
async fn test_conflict_abort() {
    let repo = TestRepo::new();
    let root = repo.worktree_root();
    let session_id = SessionId::new("sess-conflict-01");

    let worktree = create_session_worktree(repo.path(), &session_id, Some(root))
        .await
        .expect("create_session_worktree should succeed");

    // Commit conflicting change in worktree
    std::fs::write(worktree.path.join("init.txt"), "conflict from wt\n").unwrap();

    // Commit conflicting change in main
    std::fs::write(repo.path().join("init.txt"), "conflict from main\n").unwrap();
    repo.git(&["commit", "-am", "main conflicting commit"]);

    let result = finish(&worktree.path, MergeStrategy::Squash, "main").await;
    match result {
        Err(GitError::CommandFailed(msg)) => {
            assert!(msg.contains("Merge conflict"));
            assert!(msg.contains("init.txt"));
        }
        other => panic!("expected GitError::CommandFailed carrying conflicting paths, got {other:?}"),
    }

    // Worktree should still exist because merge was aborted before worktree removal
    assert!(worktree.path.exists());

    // Main repo should be completely clean and on main
    let status = repo.git(&["status", "--porcelain"]);
    assert!(status.is_empty(), "main repo should be clean after conflict abort, but was: {status}");
    let main_content = std::fs::read_to_string(repo.path().join("init.txt")).unwrap();
    assert_eq!(main_content, "conflict from main\n");
}
