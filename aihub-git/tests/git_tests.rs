use aihub_core::{MergeStrategy, SessionId};
use aihub_git::{create_session_worktree, diff, finish_session, GitError};
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
        let base = std::env::temp_dir().join(format!(
            "aihub-git-test-{}-{}-{}",
            std::process::id(),
            rand_suffix,
            count
        ));
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

    git_at(&worktree.path, &["add", "new_file.txt"]);
    let outcome = finish_session(
        &worktree.path,
        MergeStrategy::Squash,
        "main",
        &worktree.branch,
        &worktree.originating_checkout,
    )
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

    git_at(&worktree.path, &["add", "ff.txt"]);
    let outcome = finish_session(
        &worktree.path,
        MergeStrategy::FastForward,
        "main",
        &worktree.branch,
        &worktree.originating_checkout,
    )
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

    let outcome = finish_session(
        &worktree.path,
        MergeStrategy::Keep,
        "main",
        &worktree.branch,
        &worktree.originating_checkout,
    )
    .await
    .expect("keep finish should succeed");

    assert_eq!(outcome.strategy, MergeStrategy::Keep);
    assert!(outcome.success);
    assert!(worktree.path.exists());

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

    let outcome = finish_session(
        &worktree.path,
        MergeStrategy::Discard,
        "main",
        &worktree.branch,
        &worktree.originating_checkout,
    )
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

    let result = finish_session(
        &worktree.path,
        MergeStrategy::Squash,
        "main",
        &worktree.branch,
        &worktree.originating_checkout,
    )
    .await;
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

    let result = finish_session(
        &worktree.path,
        MergeStrategy::Squash,
        "main",
        &worktree.branch,
        &worktree.originating_checkout,
    )
    .await;
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

    let result = finish_session(
        &worktree.path,
        MergeStrategy::Squash,
        "main",
        &worktree.branch,
        &worktree.originating_checkout,
    )
    .await;
    match result {
        Err(GitError::CommandFailed(msg)) => {
            assert!(msg.contains("Merge conflict"));
            assert!(msg.contains("init.txt"));
        }
        other => {
            panic!("expected GitError::CommandFailed carrying conflicting paths, got {other:?}")
        }
    }

    // Worktree should still exist because merge was aborted before worktree removal
    assert!(worktree.path.exists());

    // Main repo should be completely clean and on main
    let status = repo.git(&["status", "--porcelain"]);
    assert!(
        status.is_empty(),
        "main repo should be clean after conflict abort, but was: {status}"
    );
    let main_content = std::fs::read_to_string(repo.path().join("init.txt")).unwrap();
    assert_eq!(main_content, "conflict from main\n");
}

#[tokio::test]
async fn f2_keep_preserves_ignored_and_uncommitted_work() {
    let repo = TestRepo::new();
    repo.git(&["config", "core.excludesFile", "/dev/null"]);
    std::fs::write(repo.path().join(".gitignore"), "notes.local\n").unwrap();
    repo.git(&["add", ".gitignore"]);
    repo.git(&["commit", "-m", "ignore local notes"]);
    let wt = create_session_worktree(
        repo.path(),
        &SessionId::new("keep-safe"),
        Some(repo.worktree_root()),
    )
    .await
    .unwrap();
    std::fs::write(wt.path.join("notes.local"), "valuable notes").unwrap();
    std::fs::write(wt.path.join("draft.txt"), "untracked draft").unwrap();
    std::fs::write(wt.path.join("init.txt"), "unstaged edit").unwrap();
    let before = git_at(&wt.path, &["status", "--porcelain"]);
    let head = git_at(&wt.path, &["rev-parse", "HEAD"]);
    finish_session(
        &wt.path,
        MergeStrategy::Keep,
        "main",
        &wt.branch,
        &wt.originating_checkout,
    )
    .await
    .unwrap();
    assert!(wt.path.exists(), "Keep must retain the worktree");
    assert_eq!(
        std::fs::read_to_string(wt.path.join("notes.local")).unwrap(),
        "valuable notes"
    );
    assert_eq!(git_at(&wt.path, &["status", "--porcelain"]), before);
    assert_eq!(git_at(&wt.path, &["rev-parse", "HEAD"]), head);
}

fn git_at(path: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(path)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

#[tokio::test]
async fn f2_merge_retains_and_reports_ignored_and_untracked_files() {
    for strategy in [MergeStrategy::Squash, MergeStrategy::FastForward] {
        let repo = TestRepo::new();
        std::fs::write(repo.path().join(".gitignore"), "notes.local\n").unwrap();
        repo.git(&["add", ".gitignore"]);
        repo.git(&["commit", "-m", "ignore"]);
        let wt = create_session_worktree(
            repo.path(),
            &SessionId::new("merge-safe"),
            Some(repo.worktree_root()),
        )
        .await
        .unwrap();
        std::fs::write(wt.path.join("notes.local"), "valuable").unwrap();
        std::fs::write(wt.path.join("draft.txt"), "draft").unwrap();
        std::fs::write(wt.path.join("init.txt"), "merged change").unwrap();
        let preview = diff(&wt.path, "main", false).await.unwrap();
        assert!(
            preview.contains("notes.local"),
            "Discard preview must disclose ignored paths"
        );
        assert!(preview.contains("draft.txt"));
        let out = finish_session(
            &wt.path,
            strategy,
            "main",
            &wt.branch,
            &wt.originating_checkout,
        )
        .await
        .unwrap();
        assert!(out.success);
        assert_eq!(
            std::fs::read_to_string(repo.path().join("init.txt")).unwrap(),
            "merged change"
        );
        assert_eq!(
            std::fs::read_to_string(wt.path.join("notes.local")).unwrap(),
            "valuable"
        );
        assert_eq!(
            std::fs::read_to_string(wt.path.join("draft.txt")).unwrap(),
            "draft"
        );
        assert!(out.message.contains("notes.local"));
        assert!(out.message.contains("draft.txt"));
    }
}

#[tokio::test]
async fn f3_refuses_switched_branch_for_every_strategy() {
    let repo = TestRepo::new();
    let wt = create_session_worktree(
        repo.path(),
        &SessionId::new("identity"),
        Some(repo.worktree_root()),
    )
    .await
    .unwrap();
    git_at(&wt.path, &["checkout", "-b", "feature/user-work"]);
    std::fs::write(wt.path.join("precious.txt"), "unmerged owner work").unwrap();
    git_at(&wt.path, &["add", "."]);
    git_at(&wt.path, &["commit", "-m", "owner work"]);
    let head = git_at(&wt.path, &["rev-parse", "HEAD"]);
    for strategy in [
        MergeStrategy::Discard,
        MergeStrategy::Keep,
        MergeStrategy::Squash,
        MergeStrategy::FastForward,
    ] {
        let err = finish_session(
            &wt.path,
            strategy,
            "main",
            &wt.branch,
            &wt.originating_checkout,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("feature/user-work"));
        assert!(err.contains("session/identity"));
        let err = finish_session(&wt.path, strategy, "main", &wt.branch, repo.path())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("feature/user-work"));
        assert_eq!(git_at(&wt.path, &["rev-parse", "HEAD"]), head);
    }
    assert_eq!(
        std::fs::read_to_string(wt.path.join("precious.txt")).unwrap(),
        "unmerged owner work"
    );
    assert!(repo.git(&["branch"]).contains("feature/user-work"));
}

#[tokio::test]
async fn f3_refuses_moved_or_replaced_registration() {
    let repo = TestRepo::new();
    let wt = create_session_worktree(
        repo.path(),
        &SessionId::new("moved"),
        Some(repo.worktree_root()),
    )
    .await
    .unwrap();
    let moved = repo.worktree_root().join("moved-away");
    repo.git(&[
        "worktree",
        "move",
        wt.path.to_str().unwrap(),
        moved.to_str().unwrap(),
    ]);
    // Replace recorded directory with an unrelated repository on the same branch name.
    std::fs::create_dir(&wt.path).unwrap();
    git_at(&wt.path, &["init", "-b", &wt.branch]);
    let err = finish_session(
        &wt.path,
        MergeStrategy::Discard,
        "main",
        &wt.branch,
        repo.path(),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("not registered"), "{err}");
    assert!(moved.join("init.txt").exists());
    assert!(wt.path.join(".git").exists());
}

#[tokio::test]
async fn f14_merges_into_recorded_linked_checkout() {
    for strategy in [MergeStrategy::Squash, MergeStrategy::FastForward] {
        let repo = TestRepo::new();
        let feature = repo.worktree_root().join("feature");
        repo.git(&[
            "worktree",
            "add",
            "-b",
            "feature",
            feature.to_str().unwrap(),
        ]);
        let main_head = repo.git(&["rev-parse", "HEAD"]);
        let wt = create_session_worktree(
            &feature,
            &SessionId::new("linked"),
            Some(repo.worktree_root()),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::canonicalize(&wt.originating_checkout).unwrap(),
            std::fs::canonicalize(&feature).unwrap()
        );
        std::fs::write(wt.path.join("init.txt"), "feature result").unwrap();
        let out = finish_session(
            &wt.path,
            strategy,
            &wt.base_branch,
            &wt.branch,
            &wt.originating_checkout,
        )
        .await
        .unwrap();
        assert!(out.success);
        assert_eq!(
            std::fs::read_to_string(feature.join("init.txt")).unwrap(),
            "feature result"
        );
        assert_eq!(repo.git(&["rev-parse", "HEAD"]), main_head);
        assert_eq!(
            std::fs::read_to_string(repo.path().join("init.txt")).unwrap(),
            "initial commit\n"
        );
        assert!(!wt.path.exists());
    }
}

#[tokio::test]
async fn f14_validates_originating_checkout_branch_and_cleanliness() {
    let repo = TestRepo::new();
    let feature = repo.worktree_root().join("feature");
    repo.git(&[
        "worktree",
        "add",
        "-b",
        "feature",
        feature.to_str().unwrap(),
    ]);
    let wt = create_session_worktree(
        &feature,
        &SessionId::new("linked-dirty"),
        Some(repo.worktree_root()),
    )
    .await
    .unwrap();
    std::fs::write(feature.join("init.txt"), "dirty owner work").unwrap();
    let err = finish_session(
        &wt.path,
        MergeStrategy::Squash,
        "feature",
        &wt.branch,
        &wt.originating_checkout,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("uncommitted changes"), "{err}");
    git_at(&feature, &["commit", "-am", "save owner work"]);
    git_at(&feature, &["checkout", "-b", "other"]);
    let err = finish_session(
        &wt.path,
        MergeStrategy::FastForward,
        "feature",
        &wt.branch,
        &wt.originating_checkout,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("other") && err.contains("feature"), "{err}");
    assert!(wt.path.exists());
}

#[tokio::test]
async fn f2_conflict_preserves_both_checkouts_and_session_index() {
    let repo = TestRepo::new();
    let wt = create_session_worktree(
        repo.path(),
        &SessionId::new("conflict-unchanged"),
        Some(repo.worktree_root()),
    )
    .await
    .unwrap();
    std::fs::write(wt.path.join("init.txt"), "staged session edit").unwrap();
    git_at(&wt.path, &["add", "init.txt"]);
    std::fs::write(wt.path.join("init.txt"), "unstaged session edit").unwrap();
    std::fs::write(wt.path.join("untracked"), "draft").unwrap();
    std::fs::write(repo.path().join("init.txt"), "owner committed edit").unwrap();
    repo.git(&["commit", "-am", "owner commit"]);
    let session_head = git_at(&wt.path, &["rev-parse", "HEAD"]);
    let origin_head = repo.git(&["rev-parse", "HEAD"]);
    let index_path = git_at(&wt.path, &["rev-parse", "--git-path", "index"]);
    let index = std::fs::read(wt.path.join(&index_path)).unwrap();
    let err = finish_session(
        &wt.path,
        MergeStrategy::Squash,
        "main",
        &wt.branch,
        &wt.originating_checkout,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("Merge conflict") && err.contains("init.txt"),
        "{err}"
    );
    assert_eq!(git_at(&wt.path, &["rev-parse", "HEAD"]), session_head);
    assert_eq!(repo.git(&["rev-parse", "HEAD"]), origin_head);
    assert_eq!(std::fs::read(wt.path.join(index_path)).unwrap(), index);
    assert_eq!(
        std::fs::read_to_string(wt.path.join("init.txt")).unwrap(),
        "unstaged session edit"
    );
    assert_eq!(
        std::fs::read_to_string(wt.path.join("untracked")).unwrap(),
        "draft"
    );
    assert!(repo.git(&["status", "--porcelain"]).is_empty());
}

#[tokio::test]
async fn f2_untracked_only_preserves_worktree_after_merge() {
    let repo = TestRepo::new();
    let wt = create_session_worktree(
        repo.path(),
        &SessionId::new("untracked-only"),
        Some(repo.worktree_root()),
    )
    .await
    .unwrap();
    std::fs::write(wt.path.join("draft"), "draft").unwrap();
    let out = finish_session(
        &wt.path,
        MergeStrategy::Squash,
        "main",
        &wt.branch,
        &wt.originating_checkout,
    )
    .await
    .unwrap();
    assert!(wt.path.join("draft").exists());
    assert!(out.message.contains("draft"));
    assert!(repo.path().join("draft").exists());
}

#[tokio::test]
async fn f2_ignored_only_survives_merge_and_is_listed_before_discard() {
    for strategy in [MergeStrategy::Squash, MergeStrategy::FastForward] {
        let repo = TestRepo::new();
        std::fs::write(repo.path().join(".gitignore"), "private/\n").unwrap();
        repo.git(&["add", ".gitignore"]);
        repo.git(&["commit", "-m", "ignore private directory"]);
        let wt = create_session_worktree(
            repo.path(),
            &SessionId::new("ignored-only"),
            Some(repo.worktree_root()),
        )
        .await
        .unwrap();
        std::fs::create_dir(wt.path.join("private")).unwrap();
        std::fs::write(wt.path.join("private/notes.local"), "owner notes").unwrap();
        std::fs::write(wt.path.join("init.txt"), "tracked change").unwrap();
        let preview = diff(&wt.path, "main", false).await.unwrap();
        assert!(preview.contains("private/notes.local"));
        assert!(preview.contains("Discard permanently deletes"));
        let out = finish_session(
            &wt.path,
            strategy,
            "main",
            &wt.branch,
            &wt.originating_checkout,
        )
        .await
        .unwrap();
        assert!(out.message.contains("private/notes.local"));
        assert_eq!(
            std::fs::read_to_string(wt.path.join("private/notes.local")).unwrap(),
            "owner notes"
        );
        assert_eq!(
            std::fs::read_to_string(repo.path().join("init.txt")).unwrap(),
            "tracked change"
        );
        let discarded = finish_session(
            &wt.path,
            MergeStrategy::Discard,
            "main",
            &wt.branch,
            &wt.originating_checkout,
        )
        .await
        .unwrap();
        assert!(discarded.diff.contains("private/notes.local"));
        assert!(!wt.path.exists());
        assert!(!repo.git(&["branch"]).contains(&wt.branch));
    }
}
