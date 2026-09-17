//! CLI argument parsing and repo discovery.

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug, Clone)]
#[command(
    name = "aihub",
    about = "Interactive multi-harness terminal supervisor and TUI client"
)]
pub struct Cli {
    /// Custom socket path for aihubd Unix domain socket
    #[arg(long)]
    pub socket: Option<PathBuf>,

    /// Optional task description to submit upon session startup
    #[arg(value_name = "TASK")]
    pub task: Option<String>,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Commands {
    /// Reattach to an existing session
    Attach {
        /// Optional session ID to attach to. If omitted, attaches to latest session for current repository.
        session_id: Option<String>,
    },
}

/// Discovers the git repository root for the current working directory.
pub fn detect_repo_path() -> PathBuf {
    if let Ok(output) = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
    {
        if output.status.success() {
            if let Ok(s) = std::str::from_utf8(&output.stdout) {
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    return PathBuf::from(trimmed);
                }
            }
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}
