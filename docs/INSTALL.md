# Installing aihub on macOS

This guide covers one-command setup for **aihub**, **aihubd**, and the transitional **[ai-memory](https://github.com/akitaonrails/ai-memory)** sidecar. Linux and systemd are **not** supported by these scripts yet; use `cargo install` manually on other platforms.

## Prerequisites

- macOS (Apple Silicon or Intel)
- Xcode Command Line Tools (`xcode-select --install`)
- `git`, `curl`, `shasum` (included with macOS)
- Rust **1.88+** (workspace `rust-version`; **1.98.1** is pinned in `rust-toolchain.toml`)

Ensure `~/.local/bin` is on your **interactive shell** `PATH` (LaunchAgents do not inherit your shell profile):

```bash
export PATH="$HOME/.local/bin:$PATH"
```

At install time, `./scripts/install.sh` also resolves `claude`, `codex`, `cursor-agent`, and `agy` from the installing shell and writes a LaunchAgent `PATH` for **aihubd** that includes those directories plus `/usr/bin:/bin:/usr/sbin:/sbin`. Missing harnesses produce a warning and install continues.

## Install

From a clone of this repository:

```bash
./scripts/install.sh
```

This will:

1. Verify prerequisites.
2. `cargo install --locked` **aihub** and **aihubd** into `~/.local/bin` (or copy prebuilt binaries when using `--bin-dir`).
3. Download pinned **ai-memory** release **v2.2.2**, verify SHA-256, install the binary when absent, and run `init` when needed.
4. Render and install **LaunchAgents** (via `plutil`, linted, installed atomically) for `ai-memory serve` (loopback `127.0.0.1:49374`, with `AI_MEMORY_BIND` in the plist env) and **aihubd** (socket `~/.local/share/aihub/aihub.sock`).
5. Bootstrap both agents and poll for readiness (sidecar HTTP and aihubd socket, up to 30 seconds).

Re-running install **replaces plists and restarts both services** (`launchctl bootout` / `bootstrap`). Active aihub sessions can be interrupted.

### Options

| Flag | Meaning |
|------|---------|
| `--dry-run` | Print every action without changing the system. |
| `--no-start` | Install binaries and plists but skip `launchctl` bootstrap and readiness checks. |
| `--bin-dir DIR` | Install `aihub` and `aihubd` from `DIR` instead of `cargo install` (for tests or offline bundles). |
| `--with-agent-hooks` | Opt in to `ai-memory install-mcp` / `install-hooks` for Claude Code, Codex, Cursor, and Antigravity CLI (edits harness configs). |

Example dry-run in an isolated home:

```bash
HOME="$(mktemp -d)" ./scripts/install.sh --dry-run
```

## Upgrade

Re-run `./scripts/install.sh` from an updated checkout. The script is idempotent: it reinstalls binaries, re-renders LaunchAgent plists, and reloads services (see restart note above).

To bump the bundled ai-memory version, change `AI_MEMORY_VERSION` in `scripts/install.sh` (single pin).

## Uninstall

```bash
./scripts/uninstall.sh
```

Stops LaunchAgents, removes plists, and deletes **aihub** and **aihubd** from `~/.local/bin`.

**ai-memory** is removed from `~/.local/bin` only when this installer placed it there (tracked by `~/.local/share/aihub/.installed-ai-memory-by-aihub`). A pre-existing ai-memory binary is left untouched.

**Data is kept by default.**

| Flag | Removes |
|------|---------|
| `--purge` | `~/.local/share/aihub` (daemon socket, spool, install marker). |
| `--purge-ai-memory` | `~/.local/share/ai-memory`, `~/.config/ai-memory`, `~/Library/Application Support/ai-memory`, and `~/.local/share/ai-memory-hooks`. |

Example full removal of application data:

```bash
./scripts/uninstall.sh --purge --purge-ai-memory
```

### What uninstall does not remove

- Log files under `~/Library/Logs/aihub/`
- Git repository branches, worktrees, or checkouts created by aihub
- Ephemeral worktrees under `${TMPDIR}/aihub/worktrees`
- Opt-in harness MCP/hook config entries (unless you used `--with-agent-hooks` and remove them manually)

## Logs

Services log under:

```text
~/Library/Logs/aihub/
  ai-memory.stdout.log
  ai-memory.stderr.log
  aihubd.stdout.log
  aihubd.stderr.log
```

Tail while debugging:

```bash
tail -f ~/Library/Logs/aihub/aihubd.stderr.log
```

LaunchAgent status:

```bash
launchctl print "gui/$(id -u)/io.mathborgess.aihubd"
launchctl print "gui/$(id -u)/com.github.akitaonrails.ai-memory"
```

## Troubleshooting

| Symptom | What to check |
|---------|----------------|
| `aihub` cannot connect | `launchctl print` for **aihubd**; socket at `~/.local/share/aihub/aihub.sock` should be mode `0600`. |
| Harness not found under LaunchAgent | Install from a shell whose `PATH` includes harness locations; re-run `./scripts/install.sh` after installing CLIs. |
| ai-memory `Connection refused` | **ai-memory** LaunchAgent running; `curl -s http://127.0.0.1:49374/mcp` should return a JSON-RPC error payload. |
| `cargo install` fails | Match `rust-toolchain.toml`; run `rustup toolchain install 1.98.1`. |
| Hooks not firing | Re-run with `--with-agent-hooks` or wire agents manually per [ai-memory install docs](https://github.com/akitaonrails/ai-memory/blob/main/docs/install.md). |

## CI

GitHub Actions on `macos-latest` runs `fmt`, `clippy`, `test`, `shellcheck` on `scripts/`, `scripts/test-install.sh`, and `scripts/e2e-ai-memory.sh` (see `.github/workflows/ci.yml`).
