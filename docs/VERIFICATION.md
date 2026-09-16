# Verification matrix (production run 20260915T225713Z)

Evidence from integration session 07: **198** workspace tests, gates `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --offline -- -D warnings`, `cargo test --workspace --offline --no-fail-fast` (0 failed across 3 consecutive runs), five consecutive green runs each of `cargo test -p aihubd --test regression --offline`, `cargo test -p aihub-pty --offline`, and `cargo test -p aihub-memory --offline`, plus `cargo build --release --locked --offline`, `scripts/e2e-ai-memory.sh`, `scripts/test-install.sh`, and `shellcheck scripts/*.sh` (all passed 2026-09-16). Earlier session-10 fixes (unscoped `QuotaPush`, TCP RST on fake server, stale-socket liveness) remain in place; see `docs/CONTRACT.md` §5.

## Findings F1–F14

| ID | Regression test(s) | Status |
|----|-------------------|--------|
| F1 | `f1_redacts_opaque_tokens_and_credential_assignments` (`aihub-memory/src/redact.rs`) | pass |
| F2 | `f2_keep_preserves_ignored_and_uncommitted_work`, `f2_merge_retains_and_reports_ignored_and_untracked_files`, `f2_untracked_only_preserves_worktree_after_merge`, `f2_ignored_only_survives_merge_and_is_listed_before_discard`, `f2_conflict_preserves_both_checkouts_and_session_index` (`aihub-git/tests/git_tests.rs`); `f2_f4_real_keep_and_merge_preserve_ignored_files_after_stop` (`aihubd/tests/headless_e2e.rs`) | pass |
| F3 | `f3_refuses_switched_branch_for_every_strategy`, `f3_refuses_moved_or_replaced_registration` (`aihub-git/tests/git_tests.rs`) | pass |
| F4 | `f4_redirected_descendant_ignoring_term_is_dead_before_stop_returns`, `f4_stop_error_when_signal_fails`, `f4_reap_failure_is_an_error`, `f4_stop_barrier_terminates_full_group_including_sigterm_ignoring_descendant` (`aihub-pty/tests/pty_integration.rs`); `f4_merge_refuses_when_stop_unconfirmed`, `f4_merge_ordering_through_stop_barrier`, `f4_shutdown_reaps_through_stop_barrier`, `f4_redirected_descendant_quiet_after_merge`, `f4_redirected_descendant_quiet_after_switch` (`aihubd/tests/regression.rs`); `f2_f4_real_keep_and_merge_preserve_ignored_files_after_stop` (`aihubd/tests/headless_e2e.rs`) | pass |
| F5 | `f5_switch_ordering_no_overlap`, `f5_switch_recoverable_stopped_state_on_spawn_failure`, `f5_switch_refuses_replacement_when_stop_unconfirmed`, `f5_same_harness_switch_relaunches_stopped_session` (`aihubd/tests/regression.rs`); `f5_real_switch_waits_for_sigterm_writes` (`aihubd/tests/headless_e2e.rs`) | pass |
| F6 | `f6_drop_without_stop_leaves_no_live_child_pid` (`aihub-pty/tests/pty_integration.rs`) | pass |
| F7 | `f7_stalled_writer_does_not_block_caller_or_shutdown`, `f7_full_input_queue_returns_error_without_blocking_sender` (`aihub-pty/tests/pty_integration.rs`); `f7_lock_released_before_pty_io_and_full_queue_isolated`, `f7_real_stalled_writer_isolated_in_daemon` (`aihubd/tests/regression.rs`) | pass |
| F8 | `f8_reader_stays_in_sync_when_fragmented_frame_and_key_event_interleave` (`aihub/tests/protocol.rs`) | pass |
| F9 | `f9_clear_prompt_skips_injected_fallback`, `f9_ambiguous_prompt_invokes_injected_fallback`, `f9_fallback_failure_returns_heuristic` (`aihub-router/tests/f9_fallback.rs`); `f9_task_submission_via_palette`, `f9_task_submission_via_cli_argument` (`aihub/tests/key_routing.rs`); `f9_submit_task_context_and_classify_fallback` (`aihubd/tests/regression.rs`) | pass |
| F10 | `f10_exhausted_supply_returns_no_capacity_not_a_placeholder_harness`, `f10_unknown_slots_are_never_candidates` (`aihub-router/tests/f10_route_outcome.rs`); `f10_no_capacity_never_dispatched` (`aihubd/tests/regression.rs`); `f10_accept_on_no_capacity_sends_nothing` (`aihub/tests/key_routing.rs`); `f10_real_router_exhausted_autonomous_dispatches_nothing` (`aihubd/tests/headless_e2e.rs`) | pass |
| F11 | `f11_tool_call_skipped_for_final_assistant_message` (`aihub-memory/src/transcript/codex.rs`) | pass |
| F12 | `f12_guard_restores_terminal_on_init_error` (`aihub/tests/protocol.rs`) | pass |
| F13 | `f13_other_session_events_do_not_leak` (`aihub/tests/protocol.rs`); `f13_session_scoped_events_and_attach_summary` (`aihubd/tests/regression.rs`); `f13_real_clients_receive_only_attached_session_events` (`aihubd/tests/headless_e2e.rs`) | pass |
| F14 | `f14_merges_into_recorded_linked_checkout`, `f14_validates_originating_checkout_branch_and_cleanliness` (`aihub-git/tests/git_tests.rs`) | pass |

## Findings N1–N10 (review run 2)

| ID | Regression test(s) | Status |
|----|-------------------|--------|
| N1 | `n1_silent_server_spools_within_deadline`, `n1_response_body_over_cap_spools_record` (`aihub-memory/src/ai_memory.rs`); `n1_silent_sidecar_does_not_block_other_clients_or_shutdown` (`aihubd/tests/regression.rs`) | pass |
| N2 | `n2_tool_is_error_true_spools_record`, `n2_empty_body_and_malformed_spools_record`, `n2_sse_error_spools_and_sse_success_delivers`, `n2_mismatched_id_spools_record` (`aihub-memory/src/ai_memory.rs`) | pass |
| N3 | `n3_interrupted_rewrite_keeps_pending_records`, `n3_cross_process_lock_serializes_append_and_drain` (`aihub-memory/src/ai_memory.rs`) | pass |
| N4 | `n4_deleted_brief_delivers_full_content_from_spool`, `n4_online_delivery_carries_decisions_and_identity` (`aihub-memory/src/ai_memory.rs`) | pass |
| N5 | `n5_append_over_limit_returns_spool_full_error`, `n5_timed_retry_drain_without_new_handoff` (`aihub-memory/src/ai_memory.rs`) | pass |
| N6 | `n6_hanging_list_command_times_out_and_reaps_child`, `n6_oversized_output_is_capped`, `n6_failed_refresh_keeps_last_snapshot`, `n6_route_uses_snapshot_without_io` (`aihub-router/tests/n6_catalog.rs`); `n6_hanging_model_list_does_not_block_sessions` (`aihubd/tests/regression.rs`); `antigravity_ls_address_override_prepended` (`aihub-probe/tests/cursor_agy.rs`, fixture `lsof` output) | pass |
| N7 | `test_n7_harness_path_in_plist` (`scripts/test-install.sh`) | pass |
| N8 | `lint_plists`, `assert_home_paths_rendered` (`scripts/test-install.sh`) | pass |
| N9 | `test_n9_bind_and_no_start`, `wait_for_*_ready` exercised via `scripts/install.sh` (`scripts/test-install.sh`) | pass |
| N10 | `n10_real_session_path_layout_uses_passed_project_identity` (`aihub-memory/src/ai_memory.rs`); `n10_switch_passes_repository_identity_to_memory_recorder` (`aihubd/tests/regression.rs`) | pass |

## Deviations from PLAN

| Topic | Planned | Shipped | Evidence |
|-------|---------|---------|----------|
| Palette / mode chords | Direct `Ctrl+P`, `Ctrl+M` | `Ctrl+]` prefix, then `p`/`:` for palette or `m` for mode toggle | `aihub/src/keys.rs` — `Ctrl+M` is the same byte as Enter inside the harness PTY; prefix keeps Tab, Enter, and `:` available to the child |
| Footer on narrow terminals | Full status bars | Header/footer text may clip when width is below layout minimum | TUI layout uses fixed spans; no dynamic wrap in session 06 scope |
| LLM classify fallback | Full tier + model routing | `classify_with_llm` may return tier only; daemon uses regex/`classify_with_fallback` path | `aihub-router`; autonomous model still comes from catalog snapshot when a recommendation is emitted |
| Assisted accept model id | Recommendation model on every switch path | Enter-to-accept and hold-expiry accept send `SwitchHarness.model` from the recommendation; manual `/switch` and prefix-Tab harness cycle send `model: None` | `aihub/tests/key_routing.rs` (`hold_assisted_enter…`, accept-after-hold test); `aihub/src/keys.rs` |

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
| Installer regression (isolated HOME, N7–N9) | automated | `scripts/test-install.sh` |
| Shell scripts static analysis | automated | `shellcheck scripts/*.sh` |

Install path for owners: see [INSTALL.md](INSTALL.md) and `scripts/install.sh`.
