# Requirement → test ownership

These names/paths reserve behavioral coverage for later workers; their presence
in this map is NOT a claim that those implementations/tests exist at T0. Use the
exact names for cross-worker coverage review. Feature-gated/fake-store checks
are additional to the no-default-features build gate.

## A: crates/config/tests/load_suite/mcp_config.rs

- mcp_json_is_never_written
- toml_replaces_whole_server
- disabled_override_does_not_fall_back
- expansion_default_for_unset_or_empty
- expansion_is_non_recursive
- save_rejects_stale_source_revision
- save_rejects_symlink

## B: crates/tools/tests/mcp_approval.rs

- approval_projection_tracks_launch_and_secret_references
- secret_rotation_inherited_env_and_roles_do_not_revoke_approval
- approval_preview_rejects_stale_revision
- revocation_invalidates_live_permit
- corrupt_ledger_is_unapproved

## B: crates/tools/tests/mcp_suite/mcp_secrets.rs

- secrets_absent_from_debug_events_errors_snapshots_and_ledger
- keychain_failure_never_falls_back_to_file
- unavailable_keychain_does_not_disable_literal_or_env

## C: crates/tools/tests/mcp_suite/mcp_protocol.rs

- golden_modern_http
- golden_legacy_http
- discover_unknown_error_falls_back
- discover_timeout_ignores_late_response
- modern_400_error_does_not_fall_back
- modern_headers_and_meta_agree
- x_mcp_header_mapping_is_validated
- invalid_schema_excludes_only_one_tool
- input_required_is_not_replayed

## C: crates/tools/tests/mcp_suite/mcp_http.rs

- sse_finishes_on_matching_result_without_eof
- local_exception_is_destination_bound
- dns_rebinding_requires_approval
- mcp_exception_does_not_affect_web_tools
- modern_http_closes_without_cancel_notification
- legacy_http_cancels_same_session

## D: crates/tools/tests/mcp_stdio.rs

- golden_modern_stdio
- golden_legacy_stdio
- stdio_shutdown_reaps_descendants
- stderr_does_not_deadlock
- modern_stdio_sends_cancelled
- legacy_initialize_is_not_cancelled
- unknown_call_outcome_is_not_replayed

## E2: crates/runtime/tests/web_mcp/mcp_prepare.rs

- unapproved_stdio_never_spawns
- unapproved_http_never_connects
- unapproved_server_is_skipped_and_run_continues
- worker_is_default_and_other_roles_do_not_connect
- stdio_limit_is_shared_across_project_runs_and_gui_test
- benchmark_never_prepares_mcp
- credential_failure_reasons_remain_distinct_without_fallback
- missing_environment_is_safe_and_consistent_for_env_and_args
- revocation_denies_existing_session_without_changing_snapshot
- unknown_call_is_not_replayed_and_next_explicit_call_restarts_once
- approved_names_are_published_after_executor_rebuild_without_role_leakage
- broken_toml_without_servers_is_reported_without_connection

The GUI limiter check uses the actual shared limiter at E2; E4 adds the privileged
GUI test-connection entry point. No real keychain or external service is required.

## E2: crates/tools/tests/exec_suite/executor.rs

- run_js_cannot_invoke_external_mcp (real dispatcher and fixed CALLABLE_TOOLS)

## E3: runtime restore/continue integration (pending)

- restore_registers_snapshot_without_connecting
- restore_first_call_checks_live_definition

## E: crates/tools/src/mcp/{names,registry}.rs unit tests

- external_names_use_hash_without_sanitizing
- external_names_fit_provider_constraints
- binding_is_not_reverse_parsed

## E: existing tests (preserve/migrate)

- crates/tools/tests/exec_suite/executor.rs
- crates/tools/tests/file_tools_suite/apply_patch_reserved_name.rs
- crates/runtime/tests/web_mcp/mcp_scope_e2e.rs
- crates/runtime/tests/web_mcp/mcp_scope_diagnostics/mod.rs

## F1: crates/gui/tests/settings_ui/mcp_settings.rs

- open_and_save_never_connect
- imported_edits_write_complete_toml_override
- disable_and_remove_override_are_distinct
- render_thread_does_not_perform_io

## F2: crates/gui/tests/settings_ui/mcp_actions.rs

- approval_preview_rejects_stale_revision
- unapproved_connection_test_is_refused
- token_input_persists_only_keychain_reference
- approval_does_not_mutate_current_conversation_tools
- secret_backend_errors_have_safe_status

## G: crates/runtime/tests/mcp_cache_e2e.rs

- mcp_multiple_tool_turns_preserve_frozen_wire
- mcp_discovery_order_does_not_change_snapshot
- mcp_live_changes_do_not_change_conversation_tools
- mcp_cancel_and_revoke_preserve_sent_prefix
- mcp_new_run_alone_refreshes_tools
- mcp_restore_and_continue_keep_snapshot
- mcp_compaction_and_role_switch_preserve_post_boundary_prefix
- mcp_names_preserve_api_key_and_subscription_wire

## T0 tests already implemented

- crates/tools/src/mcp/contracts_tests.rs: search_wire_helpers_preserve_existing_contract
- crates/tools/tests/mcp_suite/mcp_contracts.rs: fingerprint separation, bounded 400 envelope,
  redaction, cancellation policy golden, sticky cancellation, explicit fixture
  gate, mismatch rejection, all transcript loading, Rust stdio fixture roundtrip
- crates/runtime/tests/web_mcp/mcp_contracts.rs: inert fake-store injection, project shared
  non-waiting limiter/drop, strict saved snapshot shape, Role::config_key parity
- crates/gui/src/model/transcript/diagnostics.rs: exhaustive natural-language
  rendering of all safe McpErrorKind classes (existing test extended)

Adjacent existing search and MCP error-redaction tests retain meaningful wire
contracts and are preserved. No instruction-copy tests were introduced/removed.

## Gates

T0: cargo check --workspace --all-targets --locked; cargo check --locked -p tools
-p runtime --no-default-features; cargo nextest run --locked -p tools -E
'test(mcp) | test(search)'. Also run runtime mcp_contracts and GUI diagnostic test.

G registers mcp_cache_e2e in scripts/contract-test-filters.sh. Use input-derived
mock cache + independent assert_append_only/wire-prefix assertions, not fixed
cached-token counts. Final gates are the exact commands in plan §9. All cargo
commands set CARGO_FORK_AUTO_UPDATE=0 and RUSTC_WRAPPER=; never CARGO_TARGET_DIR.
No test sleeps, completion timeouts or wall-clock pass/fail conditions. Paused
Tokio time is for protocol deadlines/grace only; use named events/child wait.
