# ai-hub

A unified terminal supervisor and orchestration daemon for AI coding agent harnesses (**Claude Code**, **Antigravity**, **Codex**, and **Cursor Agent**).

`ai-hub` provides live quota monitoring (5-hour and 7-day rolling windows, billing cycles), intelligent prompt-tier routing (Design, Mechanical, Review), shadow Git worktree isolation, seamless harness switching with automatic handoff briefs, and a Ratatui-based TUI client with PTY multiplexing.

## Install

On macOS, install `aihub`, `aihubd`, and the transitional **ai-memory** v2.2.2 binary into `~/.local/bin`, plus optional LaunchAgents:

```bash
./scripts/install.sh
```

Dry-run (no system changes):

```bash
./scripts/install.sh --dry-run
```

Full steps, prerequisites, and uninstall: [docs/INSTALL.md](docs/INSTALL.md).

---

## Documentation

- [Product Plan (Portuguese)](docs/PLAN.md) — Architecture, quota models, and user experience.
- [System Contract](docs/CONTRACT.md) — Public crate interfaces, IPC protocol, and data invariants.
- [Verification Guide](docs/VERIFICATION.md) — Test matrix, headless e2e verification, and owner manual test steps.

---

## Building

Requires Rust 1.85+ (tested on Cargo 1.98.1):

```bash
# Build entire workspace
cargo build --workspace --offline

# Run workspace test suite
cargo test --workspace --offline

# Run workspace linter
cargo clippy --workspace --all-targets --offline -- -D warnings
```

---

## Running

### 1. Start the Daemon (`aihubd`)

The daemon manages Unix domain sockets, background PTY instances, quota refresh cycles, and Git worktrees:

```bash
# Start aihubd in the foreground (default socket: ~/.local/share/aihub/aihub.sock)
cargo run -p aihubd --

# Or specify a custom socket path
cargo run -p aihubd -- --socket /path/to/custom.sock
```

> **Note**: If `aihubd` is not running when the client launches, `aihub` will automatically start the daemon detached in the background.

### 2. Launch the TUI Client (`aihub`)

Run the client from any Git repository:

```bash
# Start new session for current repository
cargo run -p aihub --

# Reattach to an existing session or latest session for repo
cargo run -p aihub -- attach
cargo run -p aihub -- attach <session-id>
```

---

## Keyboard Controls & Command Palette

`aihub` uses `Ctrl+]` (the Telnet escape chord) as its prefix key to multiplex supervisor commands without interfering with inner harness keystrokes.

### Prefix Chords (`Ctrl+]` followed by key)

| Key | Action |
|---|---|
| `Ctrl+]` | Send literal `Ctrl+]` (byte `0x1D`) to the inner harness PTY |
| `p` or `:` | Open the interactive **Command Palette** |
| `Enter` | Accept the active assisted-mode route recommendation (with handoff) |
| `Tab` | Cycle to the next harness (`ClaudeCode` → `Antigravity` → `Codex` → `CursorAgent`) |
| `m` | Toggle mode between `[ASSISTIDO]` and `[AUTÔNOMO]` |
| `q` | Open the analytical **Quota Table** modal (5h, 7d, and billing cycle windows) |
| `d` | **Detach** client (agent continues running in background worktree) |
| *(any other)* | Cancel prefix chord and return to terminal pass-through |

### Command Palette (`/` commands)

Press `Ctrl+]` followed by `p` or `:` to open the palette:

| Command | Description |
|---|---|
| `/switch <harness>` | Switch active session to `claude`, `agy`, `codex`, or `cursor-agent` with auto handoff |
| `/merge` | Open two-step merge review dialog (unified diff + `[S] Squash` / `[F] FastForward`) |
| `/quota` | Display full quota breakdown table across all providers and lanes |
| `/mode [assisted\|autonomous]` | View or change operating mode |
| `/detach` | Safely disconnect terminal client |

---

## Workspace Architecture

```
ai-hub/
├── aihub-core/     # Core wire types, IPC messages, Base64Bytes, and default paths
├── aihub-probe/    # Quota probes (Keychain, SQLite, lsof, transcripts, window arithmetic)
├── aihub-router/   # Bilingual (EN/PT-BR) prompt classifier and tier/lane router
├── aihub-pty/      # portable-pty host, 256 KiB ring scrollback, launch recipes
├── aihub-git/      # Shadow worktree manager (tmpdir isolation, diff, squash/ff merge)
├── aihub-memory/   # Transcript extraction, credential redaction, handoff briefs, JSONL log
├── aihubd/         # Supervisor daemon on Unix domain socket (~/.local/share/aihub/aihub.sock)
└── aihub/          # Ratatui TUI client with header statusline, vt100 terminal, and dialogs
```
