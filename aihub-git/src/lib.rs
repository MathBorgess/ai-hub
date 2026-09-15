use aihub_core::paths::default_worktree_root;
use aihub_core::{MergeStrategy, SessionId};
use std::path::{Path, PathBuf};
use std::process::Output;
use thiserror::Error;
use tokio::process::Command;

#[derive(Debug, Error)]
pub enum GitError {
    #[error("I/O error executing git: {0}")]
    Io(#[from] std::io::Error),

    #[error("Git command failed: {0}")]
    CommandFailed(String),

    #[error("Invalid repository: {0}")]
    InvalidRepo(String),
}

/// Metadata about a created session worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionWorktree {
    pub session_id: SessionId,
    pub path: PathBuf,
    pub branch: String,
    pub base_branch: String,
    pub originating_checkout: PathBuf,
}

/// Outcome of finishing/merging a session worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeOutcome {
    pub strategy: MergeStrategy,
    pub success: bool,
    pub diff: String,
    pub message: String,
}

/// Runs a git command in the specified directory and returns its output.
async fn run_git(cwd: &Path, args: &[&str]) -> Result<Output, GitError> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .await?;
    Ok(output)
}

/// Runs a git command that is expected to succeed, returning its trimmed stdout.
async fn run_git_success(cwd: &Path, args: &[&str]) -> Result<String, GitError> {
    let output = run_git(cwd, args).await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let msg = if !stderr.trim().is_empty() {
            stderr.trim().to_string()
        } else {
            stdout.trim().to_string()
        };
        return Err(GitError::CommandFailed(format!(
            "git {} failed: {}",
            args.join(" "),
            msg
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Gets the current branch name of a repository or worktree.
async fn current_branch(repo_path: &Path) -> Result<String, GitError> {
    let branch = run_git_success(repo_path, &["branch", "--show-current"]).await?;
    if branch.is_empty() {
        return Err(GitError::InvalidRepo(
            "Repository is in detached HEAD state or has no current branch".to_string(),
        ));
    }
    Ok(branch)
}

/// Checks whether a repository checkout has uncommitted changes (dirty index or working tree).
async fn is_dirty(repo_path: &Path) -> Result<bool, GitError> {
    let status = run_git_success(repo_path, &["status", "--porcelain"]).await?;
    Ok(!status.trim().is_empty())
}

/// Checks whether worktree has any uncommitted changes, and commits them on the session branch.
async fn commit_worktree_changes_if_any(worktree_path: &Path) -> Result<(), GitError> {
    let status = run_git_success(worktree_path, &["status", "--porcelain"]).await?;
    if !status.trim().is_empty() {
        run_git_success(worktree_path, &["add", "-A"]).await?;
        let output = run_git(
            worktree_path,
            &["commit", "-m", "aihub: uncommitted harness work"],
        )
        .await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let msg = if !stderr.trim().is_empty() {
                stderr.trim()
            } else {
                stdout.trim()
            };
            // If git commit said nothing to commit, that's fine, otherwise error
            if !msg.contains("nothing to commit") {
                return Err(GitError::CommandFailed(format!("git commit failed: {msg}")));
            }
        }
    }
    Ok(())
}

/// Creates an ephemeral branch `session/<session-id>` and provisions a git worktree
/// at `${TMPDIR}/aihub/worktrees/<session-id>` (or under `worktree_root` if provided).
/// Owned by session 06.
pub async fn create_session_worktree(
    repo_path: &Path,
    session_id: &SessionId,
    worktree_root: Option<&Path>,
) -> Result<SessionWorktree, GitError> {
    if !repo_path.exists() {
        return Err(GitError::InvalidRepo(format!(
            "Repository path does not exist: {}",
            repo_path.display()
        )));
    }

    // Verify it is a valid git repository and discover the current branch as base
    let base_branch = match current_branch(repo_path).await {
        Ok(b) => b,
        Err(e) => {
            return Err(GitError::InvalidRepo(format!(
                "Invalid repository at {}: {}",
                repo_path.display(),
                e
            )))
        }
    };

    let target_root = match worktree_root {
        Some(p) => p.to_path_buf(),
        None => default_worktree_root(),
    };

    tokio::fs::create_dir_all(&target_root).await?;

    let worktree_path = target_root.join(session_id.as_str());
    if worktree_path.exists() {
        return Err(GitError::CommandFailed(format!(
            "Worktree path already exists: {}",
            worktree_path.display()
        )));
    }

    let branch = session_id.branch_name();

    // Create worktree with a new branch forked from base_branch
    // git worktree add -b session/<session-id> <worktree_path> <base_branch>
    let output = run_git(
        repo_path,
        &[
            "worktree",
            "add",
            "-b",
            &branch,
            &worktree_path.to_string_lossy(),
            &base_branch,
        ],
    )
    .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let msg = if !stderr.trim().is_empty() {
            stderr.trim()
        } else {
            stdout.trim()
        };
        return Err(GitError::CommandFailed(format!(
            "git worktree add failed: {msg}"
        )));
    }

    let originating_checkout =
        PathBuf::from(run_git_success(repo_path, &["rev-parse", "--show-toplevel"]).await?);
    let metadata_dir =
        PathBuf::from(run_git_success(&worktree_path, &["rev-parse", "--absolute-git-dir"]).await?);
    tokio::fs::write(metadata_dir.join("aihub-branch"), &branch).await?;
    tokio::fs::write(
        metadata_dir.join("aihub-origin"),
        originating_checkout.as_os_str().as_encoded_bytes(),
    )
    .await?;

    Ok(SessionWorktree {
        session_id: session_id.clone(),
        path: worktree_path,
        branch,
        base_branch,
        originating_checkout,
    })
}

/// Snapshot using a private index: preflight and preview must not stage or commit user work.
async fn snapshot_tree(worktree_path: &Path) -> Result<String, GitError> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let directory = std::env::temp_dir().join(format!(
        "aihub-index-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| GitError::CommandFailed(format!("snapshot clock: {e}")))?
            .as_nanos(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    ));
    tokio::fs::create_dir(&directory).await?;
    // Also cleans up when the future is cancelled.
    struct TempIndex(PathBuf);
    impl Drop for TempIndex {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let guard = TempIndex(directory);
    let index = guard.0.join("index");
    let real = run_git_success(worktree_path, &["rev-parse", "--git-path", "index"]).await?;
    let real = worktree_path.join(real);
    match tokio::fs::copy(&real, &index).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let mut tree = String::new();
    for args in [vec!["add", "-A"], vec!["write-tree"]] {
        let out = Command::new("git")
            .args(&args)
            .current_dir(worktree_path)
            .env("GIT_INDEX_FILE", &index)
            .output()
            .await?;
        if !out.status.success() {
            return Err(GitError::CommandFailed(format!(
                "snapshot git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        tree = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    }
    Ok(tree)
}

/// Paths outside the tracked snapshot, including ignored files. NUL delimiters
/// preserve whitespace/newlines; debug quoting makes the preview unambiguous.
async fn extra_paths(path: &Path) -> Result<Vec<String>, GitError> {
    let mut paths = Vec::new();
    for args in [
        vec!["ls-files", "--others", "--exclude-standard", "-z"],
        vec![
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "-z",
        ],
    ] {
        let out = run_git(path, &args).await?;
        if !out.status.success() {
            return Err(GitError::CommandFailed(
                "Cannot inventory untracked/ignored paths".into(),
            ));
        }
        paths.extend(
            out.stdout
                .split(|b| *b == 0)
                .filter(|p| !p.is_empty())
                .map(|p| format!("{:?}", String::from_utf8_lossy(p))),
        );
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// Computes the diff without touching the real index and lists paths Discard will delete.
pub async fn diff(
    worktree_path: &Path,
    base_branch: &str,
    colored: bool,
) -> Result<String, GitError> {
    let tree = snapshot_tree(worktree_path).await?;
    let mut preview = run_git_success(
        worktree_path,
        &[
            "diff",
            if colored {
                "--color=always"
            } else {
                "--no-color"
            },
            base_branch,
            &tree,
        ],
    )
    .await?;
    let extras = extra_paths(worktree_path).await?;
    if !extras.is_empty() {
        preview.push_str("\n\nUntracked/ignored paths (Discard permanently deletes these):\n");
        preview.push_str(&extras.join("\n"));
    }
    Ok(preview)
}

async fn validate_registration(origin: &Path, path: &Path, expected: &str) -> Result<(), GitError> {
    let canonical = tokio::fs::canonicalize(path).await.map_err(|e| {
        GitError::InvalidRepo(format!(
            "Recorded worktree {} is unavailable: {e}",
            path.display()
        ))
    })?;
    let out = run_git(origin, &["worktree", "list", "--porcelain", "-z"]).await?;
    if !out.status.success() {
        return Err(GitError::InvalidRepo(format!(
            "Cannot read worktree registration at {}",
            origin.display()
        )));
    }
    let mut matches = false;
    let mut registered_branch = None;
    for field in out.stdout.split(|b| *b == 0) {
        let field = String::from_utf8_lossy(field);
        if let Some(value) = field.strip_prefix("worktree ") {
            matches = tokio::fs::canonicalize(value)
                .await
                .is_ok_and(|p| p == canonical);
        } else if matches {
            if let Some(branch) = field.strip_prefix("branch refs/heads/") {
                registered_branch = Some(branch.to_owned());
            } else if field == "detached" {
                registered_branch = Some("(detached HEAD)".into());
            }
        }
    }
    let actual = registered_branch.ok_or_else(|| {
        GitError::InvalidRepo(format!(
            "Recorded worktree {} is not registered in repository at {}",
            path.display(),
            origin.display()
        ))
    })?;
    if actual != expected {
        return Err(GitError::CommandFailed(format!(
            "Worktree {} is on branch '{actual}', expected recorded branch '{expected}'",
            path.display()
        )));
    }
    // A replaced directory can still have a stale registration. Verify its actual root and branch too.
    let root = run_git_success(path, &["rev-parse", "--show-toplevel"]).await?;
    let actual = current_branch(path).await?;
    let origin_common = run_git_success(origin, &["rev-parse", "--git-common-dir"]).await?;
    let path_common = run_git_success(path, &["rev-parse", "--git-common-dir"]).await?;
    if tokio::fs::canonicalize(root).await? != canonical
        || tokio::fs::canonicalize(origin.join(origin_common)).await?
            != tokio::fs::canonicalize(path.join(path_common)).await?
    {
        return Err(GitError::InvalidRepo(format!(
            "Worktree {} no longer matches its recorded registration",
            path.display()
        )));
    }
    if actual != expected {
        return Err(GitError::CommandFailed(format!(
            "Worktree branch '{actual}' differs from recorded '{expected}'"
        )));
    }
    Ok(())
}

/// Finish only the registered session branch, merging into its recorded checkout.
pub async fn finish_session(
    worktree_path: &Path,
    strategy: MergeStrategy,
    base_branch: &str,
    expected_session_branch: &str,
    originating_checkout: &Path,
) -> Result<MergeOutcome, GitError> {
    if !expected_session_branch.starts_with("session/") || expected_session_branch == "session/" {
        return Err(GitError::InvalidRepo(format!(
            "Expected a recorded session/<id> branch, got '{expected_session_branch}'"
        )));
    }
    validate_registration(originating_checkout, worktree_path, expected_session_branch).await?;
    let diff_text = diff(worktree_path, base_branch, false).await?;
    let mut extras = extra_paths(worktree_path).await?;
    let message = match strategy {
        MergeStrategy::Keep => format!(
            "Kept branch '{expected_session_branch}' and worktree {} untouched",
            worktree_path.display()
        ),
        MergeStrategy::Discard => {
            run_git_success(
                originating_checkout,
                &[
                    "worktree",
                    "remove",
                    "--force",
                    &worktree_path.to_string_lossy(),
                ],
            )
            .await?;
            run_git_success(
                originating_checkout,
                &["branch", "-D", expected_session_branch],
            )
            .await?;
            format!("Discarded recorded branch '{expected_session_branch}' and worktree; deleted untracked/ignored paths: {}", extras.join(", "))
        }
        MergeStrategy::Squash | MergeStrategy::FastForward => {
            validate_registration(originating_checkout, originating_checkout, base_branch).await?;
            if is_dirty(originating_checkout).await? {
                return Err(GitError::CommandFailed(format!(
                    "Cannot merge: originating checkout {} has uncommitted changes",
                    originating_checkout.display()
                )));
            }
            // Preflight with an unreachable snapshot commit. Neither checkout, branch nor real
            // index is changed on conflict, including when the harness has uncommitted work.
            let tree = snapshot_tree(worktree_path).await?;
            let candidate = run_git_success(
                worktree_path,
                &[
                    "commit-tree",
                    &tree,
                    "-p",
                    "HEAD",
                    "-m",
                    "aihub merge preflight",
                ],
            )
            .await?;
            if strategy == MergeStrategy::FastForward {
                let out = run_git(
                    originating_checkout,
                    &["merge-base", "--is-ancestor", "HEAD", &candidate],
                )
                .await?;
                if !out.status.success() {
                    return Err(GitError::CommandFailed(format!("Cannot fast-forward: base branch '{base_branch}' has diverged from '{expected_session_branch}'")));
                }
            } else {
                let out = run_git(
                    originating_checkout,
                    &[
                        "merge-tree",
                        "--write-tree",
                        "--name-only",
                        "HEAD",
                        &candidate,
                    ],
                )
                .await?;
                if !out.status.success() {
                    let text = String::from_utf8_lossy(&out.stdout);
                    let conflicts: Vec<_> =
                        text.lines().skip(1).take_while(|l| !l.is_empty()).collect();
                    return Err(GitError::CommandFailed(format!(
                        "Merge conflict or preflight failure in paths: {}",
                        conflicts.join(", ")
                    )));
                }
            }
            commit_worktree_changes_if_any(worktree_path).await?;
            let mode = if strategy == MergeStrategy::Squash {
                "--squash"
            } else {
                "--ff-only"
            };
            let out = run_git(
                originating_checkout,
                &[
                    "merge",
                    mode,
                    "--no-overwrite-ignore",
                    expected_session_branch,
                ],
            )
            .await?;
            if !out.status.success() {
                // Squash has no MERGE_HEAD; reset --merge restores its clean starting index.
                if strategy == MergeStrategy::Squash {
                    run_git_success(originating_checkout, &["reset", "--merge"]).await?;
                }
                return Err(GitError::CommandFailed(format!(
                    "Merge failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
            if strategy == MergeStrategy::Squash && is_dirty(originating_checkout).await? {
                let out = run_git(
                    originating_checkout,
                    &[
                        "commit",
                        "-m",
                        &format!("Squash merge session '{expected_session_branch}'"),
                    ],
                )
                .await?;
                if !out.status.success() {
                    run_git_success(originating_checkout, &["reset", "--merge"]).await?;
                    return Err(GitError::CommandFailed(format!(
                        "Squash commit failed: {}",
                        String::from_utf8_lossy(&out.stderr).trim()
                    )));
                }
            }
            // Preserve paths seen before auto-commit, and re-inventory before cleanup.
            extras.extend(extra_paths(worktree_path).await?);
            extras.sort();
            extras.dedup();
            let mut message = format!(
                "Merged '{expected_session_branch}' into '{base_branch}' at {}",
                originating_checkout.display()
            );
            if extras.is_empty() {
                let removal = run_git(
                    originating_checkout,
                    &["worktree", "remove", &worktree_path.to_string_lossy()],
                )
                .await?;
                if removal.status.success() {
                    let delete = run_git(
                        originating_checkout,
                        &["branch", "-D", expected_session_branch],
                    )
                    .await?;
                    if !delete.status.success() {
                        message.push_str("; retained session branch (cleanup refused)");
                    }
                } else {
                    message.push_str("; retained worktree (safe cleanup refused)");
                }
            } else {
                message.push_str(&format!(
                    "; retained worktree {} and paths: {}",
                    worktree_path.display(),
                    extras.join(", ")
                ));
            }
            message
        }
    };
    Ok(MergeOutcome {
        strategy,
        success: true,
        diff: diff_text,
        message,
    })
}

/// Alias for `finish_session`.
pub async fn finish_validated(
    worktree_path: &Path,
    strategy: MergeStrategy,
    base_branch: &str,
    expected_session_branch: &str,
    originating_checkout: &Path,
) -> Result<MergeOutcome, GitError> {
    finish_session(
        worktree_path,
        strategy,
        base_branch,
        expected_session_branch,
        originating_checkout,
    )
    .await
}
