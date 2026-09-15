use std::collections::HashMap;

use aihub_core::HarnessId;

use crate::HarnessLaunchRecipe;

/// Interactive launch recipes derived from each CLI's `--help` on the build host.
///
/// - **Claude Code** (`claude`): interactive by default; an initial user message is passed as
///   the positional `[prompt]` argument (not `-p` / `--print`, which is headless).
/// - **Antigravity** (`agy`): bare `agy` starts an interactive session; an initial prompt uses
///   `--prompt-interactive` / `-i` so the first turn runs interactively and the session continues.
/// - **Codex** (`codex`): with no subcommand, options forward to the interactive TUI; an initial
///   prompt is the optional positional `[PROMPT]` argument.
/// - **Cursor Agent** (`cursor-agent` / `agent`): interactive by default; initial text is passed
///   as positional `prompt...` arguments (not `-p` / `--print`).
pub fn harness_recipe(
    harness: HarnessId,
    initial_prompt: Option<&str>,
) -> HarnessLaunchRecipe {
    match harness {
        HarnessId::ClaudeCode => {
            let mut args = Vec::new();
            if let Some(p) = initial_prompt {
                args.push(p.to_string());
            }
            HarnessLaunchRecipe {
                binary: "claude".to_string(),
                args,
                env: HashMap::new(),
            }
        }
        HarnessId::Antigravity => {
            let mut args = Vec::new();
            if let Some(p) = initial_prompt {
                args.push("--prompt-interactive".to_string());
                args.push(p.to_string());
            }
            HarnessLaunchRecipe {
                binary: "agy".to_string(),
                args,
                env: HashMap::new(),
            }
        }
        HarnessId::Codex => {
            let mut args = Vec::new();
            if let Some(p) = initial_prompt {
                args.push(p.to_string());
            }
            HarnessLaunchRecipe {
                binary: "codex".to_string(),
                args,
                env: HashMap::new(),
            }
        }
        HarnessId::CursorAgent => {
            let mut args = Vec::new();
            if let Some(p) = initial_prompt {
                args.push(p.to_string());
            }
            HarnessLaunchRecipe {
                binary: "cursor-agent".to_string(),
                args,
                env: HashMap::new(),
            }
        }
    }
}
