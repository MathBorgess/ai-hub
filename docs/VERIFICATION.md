# Verification matrix (production run 20260915T225713Z)

Evidence from integration session 07: **198** workspace tests, gates `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --offline -- -D warnings`, `cargo test --workspace --offline --no-fail-fast` (0 failed across 3 consecutive runs), five consecutive green runs each of `cargo test -p aihubd --test regression --offline`, `cargo test -p aihub-pty --offline`, and `cargo test -p aihub-memory --offline`, plus `cargo build --release --locked --offline`, `scripts/e2e-ai-memory.sh`, `scripts/test-install.sh`, and `shellcheck scripts/*.sh` (all passed 2026-09-16). Earlier session-10 fixes (unscoped `QuotaPush`, TCP RST on fake server, stale-socket liveness) remain in place; see `docs/CONTRACT.md` §5.

Evidence from integration session 13 (this round, assembling sessions 09–12 against the NO-GO review at `docs/reviews/2026-09-16-review-run3.md`): full workspace built from a clean `target/` (no seed available). `cargo fmt --all --check` clean; `cargo clippy --workspace --all-targets --offline -- -D warnings` clean; `cargo test --workspace --offline --no-fail-fast` 0 failed across 3 consecutive runs; five parallel runs each of `cargo test -p aihubd --test regression --offline`, `cargo test -p aihub-pty --offline`, `cargo test -p aihub-memory --offline` — 0 failed (a glue fix was required here, see below); the full `aihubd` regression suite 0 failed across 6 consecutive runs plus 2 more at `--test-threads=16` (8/8, matching session 12's claim); `f10_real_router_exhausted_autonomous_dispatches_nothing` 0 failed across 20 consecutive runs with 4 background `yes > /dev/null` workers on an 8-core machine; `cargo build --release --locked --offline` finished in 2m05s; `scripts/e2e-ai-memory.sh` exit 0; `scripts/test-install.sh` exit 0 (`all cases passed`); `shellcheck scripts/*.sh` clean. **Glue applied:** `aihub-memory/src/transcript/{claude,codex,antigravity}.rs` test fixtures used a fixed `std::env::temp_dir()` path shared across the crate's tests; under `cargo test -p aihub-memory` run as 5 concurrent processes (the required parallel-run proof), two of the five collided on the same path and failed with `FAILED`/panics from `.unwrap()` on a file another process had just deleted or truncated. Scoped each fixture dir with `std::process::id()`; re-ran 5x parallel afterward, 0 failed. This does not correspond to any R-finding — it is a pre-existing test-isolation gap exposed only by concurrent-process execution, not by the R1–R10 review.

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
| N9 | `test_n9_bind_and_no_start`, `test_r9_harness_executes_under_plist_path` (`scripts/test-install.sh`); `wait_for_sidecar_ready` / `wait_for_aihubd_socket_ready` in `scripts/install.sh` | partial — readiness polling loops are owner-manual (installer tests use `--no-start`, which skips bootstrap and both loops) |
| N10 | `n10_real_session_path_layout_uses_passed_project_identity` (`aihub-memory/src/ai_memory.rs`); `n10_switch_passes_repository_identity_to_memory_recorder` (`aihubd/tests/regression.rs`) | pass |

## Findings R1–R10 (review run 3, `docs/reviews/2026-09-16-review-run3.md`)

| ID | Regression test(s) | Status |
|----|-------------------|--------|
| R1 | `r1_leader_exited_descendant_quiet_after_merge`, `r1_leader_exited_descendant_quiet_after_switch`, `r1_repeated_merge_after_stop_unconfirmed_never_finishes_git`, `r1_second_client_switch_after_stop_unconfirmed_still_refuses_spawn` (`aihubd/tests/regression.rs`); `f4_redirected_descendant_leader_exits_before_stop_returns` (`aihub-pty/tests/pty_integration.rs`) | pass |
| R2 | `r2_eof_live_signal_ignoring_leader_stop_within_deadline` (`aihub-pty/tests/pty_integration.rs`) | pass |
| R3 | `r3_router_hold_deadline_through_daemon_message_and_tui_accept` (`aihub/tests/key_routing.rs`); `r3_daemon_accept_recommendation_honors_router_hold_deadline` (`aihubd/tests/regression.rs`) | pass |
| R4 | `r4_owned_drain_silent_peer_permits_simultaneous_appends_and_second_drain_with_responsive_timer` (`aihub-memory/src/ai_memory.rs`) | pass |
| R5 | `r5_null_result_rejected_and_spooled`, `r5_scalar_result_rejected_and_spooled`, `r5_sse_notification_before_result_delivered`, `r5_sse_multiline_and_multiple_events_delivered`, `r5_chunked_response_delivered`, `r5_open_sse_stream_returns_immediately_after_result`, `r5_oversized_headers_rejected` (`aihub-memory/src/ai_memory.rs`) | pass |
| R6 | `r6_partial_write_restart_and_recovery`, `r6_malformed_head_delivers_later_records_and_quarantines`, `r6_old_schema_record_migrates_and_delivers` (`aihub-memory/src/ai_memory.rs`) | pass |
| R7 | `r7_first_oversized_record_rejected`, `r7_existing_permissive_file_corrected_or_refused`, `r7_denied_permission_propagates_error` (`aihub-memory/src/ai_memory.rs`) | pass |
| R8 | `r8_lane_without_catalog_model_is_not_dispatchable` (`aihub-router/tests/routing.rs`); `r8_same_harness_model_change_passes_model_to_spawner` (`aihubd/tests/regression.rs`) | pass |
| R9 | `test_r9_harness_executes_under_plist_path` (`scripts/test-install.sh`) | pass |
| R10 | `r10_yielding_extractor_does_not_cancel_hold_dispatch`, `r10_stale_hold_timer_ignored_after_no_capacity` (`aihubd/tests/regression.rs`) | pass |

All ten regression tests ran as part of `cargo test --workspace --offline --no-fail-fast` (session 13, 3/3 clean) and the `aihubd`-suite 8/8 clean runs above; `R9` also ran inside `scripts/test-install.sh`. `PROTOCOL_VERSION` stays `2` for this round — see `docs/CONTRACT.md` §2.1 (`aihub_core::ipc`) for why that is a decision, not an oversight: the three added fields are `#[serde(default)]`-safe, and the `holds_until_s` semantic change is safe only because `scripts/install.sh` always installs `aihub`+`aihubd` together from the same build, never independently.

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
| Installer regression (isolated HOME, N7–N9, R9) | automated | `scripts/test-install.sh` |
| Installer readiness polling (`wait_for_*_ready`) | owner-manual | Install without `--no-start` on a machine with working sidecar + aihubd agents; confirm `scripts/install.sh` exits within the deadline |
| Shell scripts static analysis | automated | `shellcheck scripts/*.sh` |

Install path for owners: see [INSTALL.md](INSTALL.md) and `scripts/install.sh`.
