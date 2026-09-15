# Installing aihub on macOS

This guide covers one-command setup for **aihub**, **aihubd**, and the transitional **[ai-memory](https://github.com/akitaonrails/ai-memory)** sidecar. Linux and systemd are **not** supported by these scripts yet; use `cargo install` manually on other platforms.

## Prerequisites

- macOS (Apple Silicon or Intel)
- Xcode Command Line Tools (`xcode-select --install`)
- `git`, `curl`, `shasum` (included with macOS)
- Rust **1.88+** (workspace `rust-version`; **1.98.1** is pinned in `rust-toolchain.toml`)

Ensure `~/.local/bin` is on your `PATH`:

```bash
export PATH="$HOME/.local/bin:$PATH"
```

## Install

From a clone of this repository:

```bash
./scripts/install.sh
```

This will:

1. Verify prerequisites.
2. `cargo install --locked` **aihub** and **aihubd** into `~/.local/bin`.
3. Download pinned **ai-memory** release **v2.2.2**, verify SHA-256, install the binary, and run `init` when needed.
4. Register **LaunchAgents** for `ai-memory serve` (loopback `127.0.0.1:49374`) and **aihubd** (socket `~/.local/share/aihub/aihub.sock`).
5. Run health checks (ai-memory MCP endpoint, aihubd socket permissions).

### Options

| Flag | Meaning |
|------|---------|
| `--dry-run` | Print every action without changing the system. |
| `--with-agent-hooks` | Opt in to `ai-memory install-mcp` / `install-hooks` for Claude Code, Codex, Cursor, and Antigravity CLI (edits harness configs). |

Example dry-run in an isolated home:

```bash
HOME="$(mktemp -d)" ./scripts/install.sh --dry-run
```

## Upgrade

Re-run `./scripts/install.sh` from an updated checkout. The script is idempotent: it reinstalls binaries, re-renders LaunchAgent plists, and reloads services.

To bump the bundled ai-memory version, change `AI_MEMORY_VERSION` in `scripts/install.sh` (single pin).

## Uninstall

```bash
./scripts/uninstall.sh
```

Stops LaunchAgents, removes plists, and deletes `aihub`, `aihubd`, and `ai-memory` from `~/.local/bin`. **Data is kept** unless you pass `--purge`:

```bash
./scripts/uninstall.sh --purge
```

`--purge` removes `~/.local/share/aihub`, `~/.local/share/ai-memory`, `~/.config/ai-memory`, and `~/Library/Application Support/ai-memory`.

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
| ai-memory `Connection refused` | **ai-memory** LaunchAgent running; `curl -s http://127.0.0.1:49374/mcp` should return a JSON-RPC error payload. |
| `cargo install` fails | Match `rust-toolchain.toml`; run `rustup toolchain install 1.98.1`. |
| Hooks not firing | Re-run with `--with-agent-hooks` or wire agents manually per [ai-memory install docs](https://github.com/akitaonrails/ai-memory/blob/main/docs/install.md). |

## CI

GitHub Actions on `macos-latest` runs `fmt`, `clippy`, `test`, and `shellcheck` on `scripts/` (see `.github/workflows/ci.yml`).
