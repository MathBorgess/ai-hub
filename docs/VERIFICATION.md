# Verification matrix (production run 20260915T182254Z)

Evidence from session 10's correction pass: **159** workspace tests, gates `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --offline -- -D warnings`, `cargo test --workspace --offline --no-fail-fast` (0 failed across 3 consecutive runs), and `cargo build --release --locked --offline` (all passed on 2026-09-15). Three previously-flaky regression areas were root-caused and fixed rather than serialized away — see `docs/CONTRACT.md` §5 and the session 10 result for detail: an unscoped `QuotaPush` broadcast interleaving with RPC-style replies in `aihubd/tests/regression.rs`, a fake test server in `aihub-memory/src/ai_memory.rs` closing sockets with unread request bytes (TCP RST), and a kernel backlog false-positive in `aihubd`'s stale-socket liveness check.

## Findings F1–F14

| ID | Regression test(s) | Status |
|----|-------------------|--------|
| F1 | `f1_redacts_opaque_tokens_and_credential_assignments` (`aihub-memory/src/redact.rs`) | pass |
| F2 | `f2_keep_preserves_ignored_and_uncommitted_work`, `f2_merge_retains_and_reports_ignored_and_untracked_files`, `f2_untracked_only_preserves_worktree_after_merge`, `f2_ignored_only_survives_merge_and_is_listed_before_discard`, `f2_conflict_preserves_both_checkouts_and_session_index` (`aihub-git/tests/git_tests.rs`); `f2_f4_real_keep_and_merge_preserve_ignored_files_after_stop` (`aihubd/tests/headless_e2e.rs`) | pass |
| F3 | `f3_refuses_switched_branch_for_every_strategy`, `f3_refuses_moved_or_replaced_registration` (`aihub-git/tests/git_tests.rs`) | pass |
| F4 | `f4_stop_barrier_terminates_full_group_including_sigterm_ignoring_descendant` (`aihub-pty/tests/pty_integration.rs`); `f4_merge_ordering_through_stop_barrier`, `f4_shutdown_reaps_through_stop_barrier` (`aihubd/tests/regression.rs`); `f2_f4_real_keep_and_merge_preserve_ignored_files_after_stop` (`aihubd/tests/headless_e2e.rs`) | pass |
| F5 | `f5_switch_ordering_no_overlap`, `f5_switch_recoverable_stopped_state_on_spawn_failure` (`aihubd/tests/regression.rs`); `f5_real_switch_waits_for_sigterm_writes` (`aihubd/tests/headless_e2e.rs`) | pass |
| F6 | `f6_drop_without_stop_leaves_no_live_child_pid` (`aihub-pty/tests/pty_integration.rs`) | pass |
| F7 | `f7_full_input_queue_returns_error_without_blocking_sender` (`aihub-pty/tests/pty_integration.rs`); `f7_lock_released_before_pty_io_and_full_queue_isolated` (`aihubd/tests/regression.rs`) | pass |
| F8 | `f8_reader_stays_in_sync_when_fragmented_frame_and_key_event_interleave` (`aihub/tests/protocol.rs`) | pass |
| F9 | `f9_clear_prompt_skips_injected_fallback`, `f9_ambiguous_prompt_invokes_injected_fallback`, `f9_fallback_failure_returns_heuristic` (`aihub-router/tests/f9_fallback.rs`); `f9_task_submission_via_palette`, `f9_task_submission_via_cli_argument` (`aihub/tests/key_routing.rs`); `f9_submit_task_context_and_classify_fallback` (`aihubd/tests/regression.rs`) | pass |
| F10 | `f10_exhausted_supply_returns_no_capacity_not_a_placeholder_harness`, `f10_unknown_slots_are_never_candidates` (`aihub-router/tests/f10_route_outcome.rs`); `f10_no_capacity_never_dispatched` (`aihubd/tests/regression.rs`); `f10_accept_on_no_capacity_sends_nothing` (`aihub/tests/key_routing.rs`); `f10_real_router_exhausted_autonomous_dispatches_nothing` (`aihubd/tests/headless_e2e.rs`) | pass |
| F11 | `f11_tool_call_skipped_for_final_assistant_message` (`aihub-memory/src/transcript/codex.rs`) | pass |
| F12 | `f12_guard_restores_terminal_on_init_error` (`aihub/tests/protocol.rs`) | pass |
| F13 | `f13_other_session_events_do_not_leak` (`aihub/tests/protocol.rs`); `f13_session_scoped_events_and_attach_summary` (`aihubd/tests/regression.rs`); `f13_real_clients_receive_only_attached_session_events` (`aihubd/tests/headless_e2e.rs`) | pass |
| F14 | `f14_merges_into_recorded_linked_checkout`, `f14_validates_originating_checkout_branch_and_cleanliness` (`aihub-git/tests/git_tests.rs`) | pass |

## PLAN §4 — automated vs owner-manual

| Item | Kind | Command or steps |
|------|------|------------------|
| Probe window/lane math (unit) | automated | `cargo test -p aihub-probe --offline` |
| Router Mechanical/Design/Review battery | automated | `cargo test -p aihub-router --offline` |
| Git worktree isolation | automated | `cargo test -p aihub-git --offline` |
| Full workspace regression | automated | `cargo test --workspace --offline` |
| Lint / format | automated | `cargo fmt --check`; `cargo clippy --workspace --all-targets --offline -- -D warnings` |
| Release binaries | automated | `cargo build --release --locked --offline` |
| ai-memory record → CLI handoffs → spool → drain (loopback, temp dirs) | automated (session 10) | `scripts/e2e-ai-memory.sh` — one command: reuses `scripts/install.sh`'s pinned-asset download/checksum, inits a temp data dir, serves on a free loopback port, runs `session10_ai_memory_live_record_spool_and_drain`, and always tears down via `trap` |
| Daemon socket permissions | automated | `cargo test -p aihubd --test headless_e2e test_headless_end_to_end --offline` |
| E2E merge / switch / routing (headless) | automated | `cargo test -p aihubd --test headless_e2e --offline`; `cargo test -p aihubd --test regression --offline` |
| TUI + live Keychain quotas | owner-manual | Run `aihub` in a real repo; authorize Keychain; confirm statusline and `/quota` table (`docs/INSTALL.md`) |
| Live harness PTY colors | owner-manual | `aihub` with `claude` / `agy` / `codex` / `cursor-agent` (not run in CI) |
| Terminal detach/reattach in real terminal | owner-manual | Close terminal, `aihub attach`, confirm scrollback |
| Live `/switch` handoff across providers | owner-manual | `/switch` with real harnesses and quota spend |
| Live `/merge` squash on owner branch | owner-manual | `/merge` in TUI on a real repo |

Install path for owners: see [INSTALL.md](INSTALL.md) and `scripts/install.sh`.
