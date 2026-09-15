use std::path::{Path, PathBuf};
use std::process::Output;
use aihub_core::paths::default_worktree_root;
use aihub_core::{MergeStrategy, SessionId};
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

/// Discovers the main repository working directory for a worktree.
/// In `git worktree list --porcelain`, the first entry is always the main worktree.
async fn find_main_repo_path(worktree_path: &Path) -> Result<PathBuf, GitError> {
    let output = run_git_success(worktree_path, &["worktree", "list", "--porcelain"]).await?;
    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("worktree ") {
            let path = PathBuf::from(rest.trim());
            if path.exists() {
                return Ok(path);
            }
        }
    }
    Err(GitError::InvalidRepo(
        "Could not determine main repository from worktree".to_string(),
    ))
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

    Ok(SessionWorktree {
        session_id: session_id.clone(),
        path: worktree_path,
        branch,
        base_branch,
    })
}

/// Computes the git diff of the worktree against its base branch.
/// Owned by session 06.
pub async fn diff(
    worktree_path: &Path,
    base_branch: &str,
    colored: bool,
) -> Result<String, GitError> {
    if !worktree_path.exists() {
        return Err(GitError::InvalidRepo(format!(
            "Worktree path does not exist: {}",
            worktree_path.display()
        )));
    }

    // Use a temporary index file so uncommitted work (including untracked files)
    // is captured in diff without modifying the actual worktree index or files.
    let rand_suffix: u64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let temp_index_path = std::env::temp_dir().join(format!("aihub-diff-index-{}-{}", std::process::id(), rand_suffix));

    // Copy existing worktree index if it exists
    let real_git_path = run_git_success(worktree_path, &["rev-parse", "--git-path", "index"]).await?;
    let real_index = if Path::new(&real_git_path).is_absolute() {
        PathBuf::from(&real_git_path)
    } else {
        worktree_path.join(&real_git_path)
    };

    if real_index.exists() {
        let _ = tokio::fs::copy(&real_index, &temp_index_path).await;
    }

    let mut add_cmd = Command::new("git");
    add_cmd
        .args(["add", "-A"])
        .current_dir(worktree_path)
        .env("GIT_INDEX_FILE", &temp_index_path);
    let add_res = add_cmd.output().await?;
    if !add_res.status.success() {
        let stderr = String::from_utf8_lossy(&add_res.stderr);
        return Err(GitError::CommandFailed(format!(
            "git add in temp index failed: {}",
            stderr.trim()
        )));
    }

    let mut write_tree_cmd = Command::new("git");
    write_tree_cmd
        .arg("write-tree")
        .current_dir(worktree_path)
        .env("GIT_INDEX_FILE", &temp_index_path);
    let write_tree_res = write_tree_cmd.output().await?;
    if !write_tree_res.status.success() {
        let stderr = String::from_utf8_lossy(&write_tree_res.stderr);
        return Err(GitError::CommandFailed(format!(
            "git write-tree failed: {}",
            stderr.trim()
        )));
    }
    let tree_hash = String::from_utf8_lossy(&write_tree_res.stdout)
        .trim()
        .to_string();

    let color_arg = if colored {
        "--color=always"
    } else {
        "--no-color"
    };

    let diff_output = run_git_success(
        worktree_path,
        &["diff", color_arg, base_branch, &tree_hash],
    )
    .await;

    let _ = tokio::fs::remove_file(&temp_index_path).await;

    diff_output
}

/// Finishes the session worktree applying the selected strategy (squash, fast-forward, keep, discard).
/// Owned by session 06.
pub async fn finish(
    worktree_path: &Path,
    strategy: MergeStrategy,
    base_branch: &str,
) -> Result<MergeOutcome, GitError> {
    if !worktree_path.exists() {
        return Err(GitError::InvalidRepo(format!(
            "Worktree path does not exist: {}",
            worktree_path.display()
        )));
    }

    let session_branch = current_branch(worktree_path).await?;
    let main_repo = find_main_repo_path(worktree_path).await?;

    match strategy {
        MergeStrategy::Keep => {
            // Commit uncommitted changes in worktree if any, and compute diff
            commit_worktree_changes_if_any(worktree_path).await?;
            let diff_text = diff(worktree_path, base_branch, false).await?;

            // Remove worktree without deleting the branch
            run_git_success(
                &main_repo,
                &["worktree", "remove", "--force", &worktree_path.to_string_lossy()],
            )
            .await?;

            Ok(MergeOutcome {
                strategy,
                success: true,
                diff: diff_text,
                message: format!("Kept branch '{session_branch}' and removed worktree"),
            })
        }
        MergeStrategy::Discard => {
            // Discard touches only session's own worktree and branch
            let diff_text = diff(worktree_path, base_branch, false).await.unwrap_or_default();

            // Remove worktree force
            run_git_success(
                &main_repo,
                &["worktree", "remove", "--force", &worktree_path.to_string_lossy()],
            )
            .await?;

            // Delete session branch
            run_git_success(&main_repo, &["branch", "-D", &session_branch]).await?;

            Ok(MergeOutcome {
                strategy,
                success: true,
                diff: diff_text,
                message: format!("Discarded session branch '{session_branch}' and removed worktree"),
            })
        }
        MergeStrategy::FastForward | MergeStrategy::Squash => {
            // Refuse to merge if the main repo has uncommitted changes
            if is_dirty(&main_repo).await? {
                return Err(GitError::CommandFailed(
                    "Cannot merge: main repository has uncommitted changes".to_string(),
                ));
            }

            // Refuse to merge if the main repo has moved to another branch
            let main_branch = current_branch(&main_repo).await?;
            if main_branch != base_branch {
                return Err(GitError::CommandFailed(format!(
                    "Cannot merge: main repository is on branch '{main_branch}', expected base branch '{base_branch}'"
                )));
            }

            // Commit uncommitted harness work into the worktree session branch
            commit_worktree_changes_if_any(worktree_path).await?;

            // Calculate diff before removing worktree
            let diff_text = diff(worktree_path, base_branch, false).await?;

            if strategy == MergeStrategy::FastForward {
                // Check if fast-forward is possible (main is ancestor of session_branch)
                let ff_check = run_git(&main_repo, &["merge-base", "--is-ancestor", base_branch, &session_branch]).await?;
                if !ff_check.status.success() {
                    return Err(GitError::CommandFailed(format!(
                        "Cannot fast-forward: base branch '{base_branch}' has diverged from session branch '{session_branch}'"
                    )));
                }

                // Remove worktree before fast-forwarding the branch
                run_git_success(
                    &main_repo,
                    &["worktree", "remove", "--force", &worktree_path.to_string_lossy()],
                )
                .await?;

                // Merge ff-only
                run_git_success(&main_repo, &["merge", "--ff-only", &session_branch]).await?;

                // Delete session branch
                let _ = run_git(&main_repo, &["branch", "-d", &session_branch]).await;

                Ok(MergeOutcome {
                    strategy,
                    success: true,
                    diff: diff_text,
                    message: format!("Fast-forward merged '{session_branch}' into '{base_branch}'"),
                })
            } else {
                // MergeStrategy::Squash
                // Test for merge conflict first using git merge-tree --write-tree
                let merge_tree_output = run_git(&main_repo, &["merge-tree", "--write-tree", "--name-only", base_branch, &session_branch]).await?;
                if !merge_tree_output.status.success() {
                    let stdout = String::from_utf8_lossy(&merge_tree_output.stdout);
                    // Extract conflicting file paths
                    let mut conflicting_paths = Vec::new();
                    let lines: Vec<&str> = stdout.lines().collect();
                    for line in lines.iter().skip(1) {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            break;
                        }
                        conflicting_paths.push(trimmed.to_string());
                    }
                    let err_msg = if conflicting_paths.is_empty() {
                        format!("Merge conflict between '{base_branch}' and '{session_branch}'")
                    } else {
                        format!(
                            "Merge conflict between '{}' and '{}' in paths: {}",
                            base_branch,
                            session_branch,
                            conflicting_paths.join(", ")
                        )
                    };
                    return Err(GitError::CommandFailed(err_msg));
                }

                // Remove worktree
                run_git_success(
                    &main_repo,
                    &["worktree", "remove", "--force", &worktree_path.to_string_lossy()],
                )
                .await?;

                // Execute squash merge in main_repo
                let squash_res = run_git(&main_repo, &["merge", "--squash", &session_branch]).await?;
                if !squash_res.status.success() {
                    // Collect conflict paths if any
                    let conflict_paths_res = run_git(&main_repo, &["diff", "--name-only", "--diff-filter=U"]).await?;
                    let conflicting = String::from_utf8_lossy(&conflict_paths_res.stdout)
                        .lines()
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>();

                    // Abort merge cleanly using git reset --merge
                    let _ = run_git(&main_repo, &["reset", "--merge"]).await;

                    let msg = if conflicting.is_empty() {
                        "Squash merge failed".to_string()
                    } else {
                        format!("Merge conflict in paths: {}", conflicting.join(", "))
                    };
                    return Err(GitError::CommandFailed(msg));
                }

                // Commit squashed changes
                let commit_res = run_git(
                    &main_repo,
                    &["commit", "-m", &format!("Squash merge session '{session_branch}'")],
                )
                .await?;

                if !commit_res.status.success() {
                    let stdout = String::from_utf8_lossy(&commit_res.stdout);
                    if !stdout.contains("nothing to commit") {
                        let stderr = String::from_utf8_lossy(&commit_res.stderr);
                        let _ = run_git(&main_repo, &["reset", "--merge"]).await;
                        return Err(GitError::CommandFailed(format!("git commit squash failed: {stderr}")));
                    }
                }

                // Delete session branch
                let _ = run_git(&main_repo, &["branch", "-D", &session_branch]).await;

                Ok(MergeOutcome {
                    strategy,
                    success: true,
                    diff: diff_text,
                    message: format!("Squash merged '{session_branch}' into '{base_branch}'"),
                })
            }
        }
    }
}
