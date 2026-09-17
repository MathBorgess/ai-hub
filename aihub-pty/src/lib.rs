use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use aihub_core::HarnessId;
use thiserror::Error;

mod harness;
mod scrollback;
mod spawn;

#[derive(Debug, Error)]
pub enum PtyError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("PTY system error: {0}")]
    Pty(String),

    #[error("Process not running or already terminated")]
    NotRunning,

    #[error("Input queue is full")]
    QueueFull,

    #[error("Stop barrier timed out before the process group was confirmed dead")]
    StopTimeout,

    #[error("Stop barrier signal failed: {0}")]
    StopSignal(String),

    #[error("Stop barrier failed to reap child: {0}")]
    StopReap(String),
}

/// Terminal window dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtySize {
    pub cols: u16,
    pub rows: u16,
}

impl Default for PtySize {
    fn default() -> Self {
        Self { cols: 80, rows: 24 }
    }
}

/// Configuration options for spawning a child process inside a virtual PTY.
#[derive(Debug, Clone)]
pub struct PtySpawnOptions {
    pub cwd: PathBuf,
    pub env: HashMap<String, String>,
    pub size: PtySize,
    pub initial_prompt: Option<String>,
}

/// Handle to an active PTY session running a command or harness.
pub struct PtyHandle {
    pub(crate) inner: Arc<spawn::PtyInner>,
}

/// Spawns an arbitrary command inside a new PTY.
pub fn spawn_command(
    cmd: &str,
    args: &[&str],
    opts: PtySpawnOptions,
) -> Result<PtyHandle, PtyError> {
    spawn::spawn_command(cmd, args, opts)
}

/// Spawns an agent harness using its standard launch recipe inside a new PTY.
pub fn spawn_harness(
    harness: HarnessId,
    opts: PtySpawnOptions,
    model: Option<&str>,
) -> Result<PtyHandle, PtyError> {
    let recipe = harness::harness_recipe_with_model(harness, opts.initial_prompt.as_deref(), model);
    let spawn_opts = spawn::merge_spawn_opts(opts.cwd, opts.size, recipe.env, opts.env, None);
    let arg_refs: Vec<&str> = recipe.args.iter().map(String::as_str).collect();
    spawn_command(&recipe.binary, &arg_refs, spawn_opts)
}

/// Command, arguments, and environment variables needed to launch a harness interactively.
#[derive(Debug, Clone, PartialEq)]
pub struct HarnessLaunchRecipe {
    pub binary: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
}

/// Generates the interactive launch recipe for a specific harness.
///
/// See `harness::harness_recipe` for how each `HarnessId` passes an initial prompt on the CLI.
pub fn harness_recipe(harness: HarnessId, initial_prompt: Option<&str>) -> HarnessLaunchRecipe {
    harness::harness_recipe(harness, initial_prompt)
}

/// Generates the interactive launch recipe for a specific harness with an optional model identifier (plan §3.3).
///
/// See `harness::harness_recipe_with_model` for the `--model` flag used by each `HarnessId`.
pub fn harness_recipe_with_model(
    harness: HarnessId,
    initial_prompt: Option<&str>,
    model: Option<&str>,
) -> HarnessLaunchRecipe {
    harness::harness_recipe_with_model(harness, initial_prompt, model)
}
