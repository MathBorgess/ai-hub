# Verification Report: ai-hub

This document marks each item from `docs/PLAN.md` §4 as either **passed-headless** (verified during automated and headless suite) or **manual-for-owner** (requires live credentials, macOS Keychain GUI prompts, or spend of provider quotas), providing exact reproduction commands and verification steps for each.

---

## 1. Automated Test Suite (PLAN §4: Testes automatizados)

All automated crate suites pass completely offline with zero network and fabricated credentials/fixtures.

### Summary Table

| Subsystem / Crate | Purpose | Status | Command |
|---|---|---|---|
| `aihub-core` | Core IPC, wire types, serialization roundtrips | **passed-headless** (5/5 tests) | `cargo test -p aihub-core --offline` |
| `aihub-probe` | Credential discovery, usage parsing, window math, SQLite, lsof | **passed-headless** (20/20 tests) | `cargo test -p aihub-probe --offline` |
| `aihub-router` | EN/PT-BR classifier, heuristic latency, lane routing engine | **passed-headless** (16/16 tests) | `cargo test -p aihub-router --offline` |
| `aihub-pty` | portable-pty wrapper, byte ring scrollback, recipes | **passed-headless** (7/7 tests) | `cargo test -p aihub-pty --offline` |
| `aihub-git` | Shadow worktrees, branch isolation, diff, squash/ff finish | **passed-headless** (9/9 tests) | `cargo test -p aihub-git --offline` |
| `aihub-memory` | Transcript extraction, secret redaction, handoff briefs, JSONL | **passed-headless** (8/8 tests) | `cargo test -p aihub-memory --offline` |
| `aihubd` | Unix domain socket daemon, session lifecycle, merge orchestration | **passed-headless** (10/10 tests) | `cargo test -p aihubd --offline` |
| `aihub` | Ratatui TUI client, key routing, rendering, IPC handshake | **passed-headless** (19/19 tests) | `cargo test -p aihub --offline` |
| **Workspace Total** | Full workspace test suite | **passed-headless** (89/89 tests) | `cargo test --workspace --offline` |
| **Clippy Check** | Linter across all targets and crates (`-D warnings`) | **passed-headless** (0 warnings) | `cargo clippy --workspace --all-targets --offline -- -D warnings` |

---

## 2. End-to-End Verification (PLAN §4: Verificação manual ponta a ponta)

### Item 1: Daemon Unix Socket & Private Permissions
- **Status:** **passed-headless**
- **Headless Evidence:** Verified in `aihubd/tests/headless_e2e.rs` (`test_headless_end_to_end`) and `aihubd/tests/socket.rs` (`stale_socket_permissions_and_shutdown`). The daemon creates its parent directory with mode `0o700` (`rwx------`) and the socket file with mode `0o600` (`rw-------`).
- **Owner Manual Command:**
  ```bash
  cargo run -p aihubd -- --socket /tmp/aihub-test.sock
  ```
  In a separate terminal, verify permissions and file type:
  ```bash
  ls -ld $(dirname /tmp/aihub-test.sock)
  ls -la /tmp/aihub-test.sock
  # Confirm socket file has srw------- (0600) and parent has drwx------ (0700)
  ```

### Item 2: TUI and Quotas (Statusline & Analytical Table)
- **Status:** **manual-for-owner**
- **Rationale:** Reading live quotas touches the macOS Keychain (`SecKeychain::find_generic_password` for Claude Code credentials and Antigravity tokens) and queries active local language servers or vendor endpoints. In headless environments, macOS Keychain access invokes an OS GUI confirmation dialog that blocks indefinitely.
- **Owner Manual Steps:**
  1. Open a terminal in any Git repository checkout.
  2. Run the supervisor client (which automatically launches `aihubd` if not already running):
     ```bash
     cargo run -p aihub --
     ```
  3. Authorize macOS Keychain prompts if requested by the system.
  4. **Statusline Check:** Look at the top status bar:
     - Verify colored quota bars for configured slots:
       - `CLAUDE` (Green <70%, Yellow 70-90%, Red >90% or exhausted).
       - `AGY-G` (Antigravity Gemini lane) and `AGY-3` (Antigravity third-party lane).
       - `CURS` (Cursor frontier/other lanes).
     - Verify active mode badge `[ASSISTIDO]` or `[AUTÔNOMO]`.
     - Verify current branch and harness identifier.
  5. **Analytical Table Check:**
     - Press `Ctrl+]` followed by `q` (or `Ctrl+]` then `:` and type `/quota` + `Enter`).
     - A modal table displays all probed windows (5-hour rolling, 7-day heaviest, monthly billing cycle), exact used/remaining percentages, resets, and status buckets.
     - Press `Esc` to close the modal.

### Item 3: Interactive PTY Execution & Terminal Hygiene
- **Status:** **manual-for-owner** for live harnesses; **passed-headless** for virtual PTY execution.
- **Headless Evidence:** Headless execution tested with `/bin/sh` in `aihubd/tests/headless_e2e.rs`: commands passed through PTY, ANSI output rendered, window resized, and output streamed in real time.
- **Rationale for Manual:** Live harnesses (`claude`, `agy`, `codex`, `cursor-agent`) consume provider account quotas and require interactive login.
- **Owner Manual Steps:**
  1. Run `cargo run -p aihub --`.
  2. Type a message or command (e.g. asking for a code explanation).
  3. Verify:
     - Keystrokes pass through to the underlying harness without delay.
     - Terminal emulation (`vt100` engine) displays syntax highlighting and terminal colors accurately.
     - Resizing your terminal window dynamically triggers `PtyResize` in `aihubd` and adjusts the inner terminal layout.

### Item 4: Detach and Reattach Scrollback Replay
- **Status:** **passed-headless**
- **Headless Evidence:** Verified in `aihubd/tests/headless_e2e.rs`. A client attached to a session running `/bin/sh`, sent `echo 'HELLO_AIHUB_HEADLESS_E2E'`, detached, disconnected, and a second client reattached. The daemon replayed the entire 256 KiB scrollback buffer containing the marker before streaming live bytes.
- **Owner Manual Steps:**
  1. In terminal 1, run `cargo run -p aihub --`.
  2. Send some commands so output appears on the terminal.
  3. Press `Ctrl+]` followed by `d` to detach (or press `Ctrl+C` / close terminal tab). The background daemon `aihubd` and running harness continue executing unhindered.
  4. In terminal 2, run:
     ```bash
     cargo run -p aihub -- attach
     ```
  5. Verify that the previous session terminal output re-renders immediately from scrollback, and live input resumes seamlessly.

### Item 5: Switch Harness and Automatic Handoff
- **Status:** **manual-for-owner** for live harnesses; **passed-headless** for daemon switch and brief generation.
- **Headless Evidence:**
  - `aihub-memory` test `writes_numbered_brief_pair_with_redaction` verifies that `write_brief_pair` creates numbered `NN.md` and `NN.prompt.md` files with secret token redaction.
  - `aihub-memory` test `appends_jsonl_in_temp_data_dir` verifies append-only logging to `handoffs.jsonl`.
  - `aihubd/tests/socket.rs` test `explicit_switch_starts_in_same_worktree_before_killing_outgoing` validates that switching harnesses starts the incoming harness in the same worktree before terminating the outgoing harness, preserving uncommitted edits.
- **Owner Manual Steps:**
  1. In an active session (e.g., running `claude`), press `Ctrl+]` then `:` (or `p`) to open the command palette.
  2. Type `/switch agy` and press `Enter`.
  3. Verify:
     - Outgoing harness transcript is parsed, extracting the last assistant response and decisions.
     - Any bearer tokens, JWTs, or API keys are redacted with `[REDACTED]`.
     - Handoff brief files `NN.md` and `NN.prompt.md` are written to `~/.local/share/aihub/briefs/<session-id>/`.
     - Metadata record is appended to `~/.local/share/aihub/handoffs.jsonl`.
     - `agy` launches inside the exact same shadow worktree, preloaded with the brief context.

### Item 6: Worktree Merge Flow (Two-Step Confirmation & Squash Merge)
- **Status:** **passed-headless**
- **Headless Evidence:** Verified in `aihubd/tests/headless_e2e.rs`. A shadow worktree was initialized in a temporary repository, a new file `new_feature.txt` was written, the client sent a `MergeRequest`, the daemon responded with a preview diff and `success: false` instructing review, the client confirmed with a matching `MergeRequest`, the daemon executed `aihub_git::finish(..., Squash)`, and the main checkout received the squash commit.
- **Owner Manual Steps:**
  1. In an active session, press `Ctrl+]` then `:` (or `p`).
  2. Type `/merge` and press `Enter`.
  3. The TUI displays the two-step merge review dialog:
     - Review the syntax-colored unified git diff against the base branch.
     - Choose a merge strategy: `[S] Squash` (default), `[F] Fast-Forward`, `[K] Keep Branch`, `[D] Discard`.
  4. Press `s` (or `Enter`) to confirm the squash merge.
  5. Check `git log` and `git status` in your main repository to verify that changes have been squash-merged cleanly and the temporary shadow worktree removed.
