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

    /// Daemon target: a local socket (`unix:<path>` or bare path) or a remote
    /// WebSocket URL (`wss://host:port`). Remote targets never autostart a
    /// local `aihubd` (design doc §2.7). Falls back to `AIHUB_DAEMON` when
    /// omitted (clap's `env` attribute needs a `clap` feature this workspace
    /// does not enable, so the fallback is applied after parsing — see
    /// `Cli::daemon_target`).
    #[arg(long)]
    pub daemon: Option<String>,

    /// Disable autostart of a local `aihubd` when the local socket is unreachable.
    /// Has no effect with a remote `--daemon` target, which never autostarts.
    #[arg(long)]
    pub no_autostart: bool,

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
    /// Diagnose pairing, route, protocol version, and clock skew (ADR gap §4.1).
    Doctor {
        /// Inspect the remote daemon target instead of the local socket.
        #[arg(long)]
        remote: bool,
    },
}

impl Cli {
    /// The effective `--daemon` target: the flag if given, else `AIHUB_DAEMON`.
    pub fn daemon_target(&self) -> Option<String> {
        self.daemon
            .clone()
            .or_else(|| std::env::var("AIHUB_DAEMON").ok())
    }
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
