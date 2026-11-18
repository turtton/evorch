#!/usr/bin/env bash
# Keep the offline gates and the workspace remainder disjoint. nextest isolates
# each test process, including providers' process-global cache tests.
# cache_preservation_e2e includes approval_cache::approved_tool_preserves_next_provider_prefix
# (Chat Completions, Responses and Codex; approval is not a cache boundary).
# The entire cache_preservation_e2e binary also registers plan_cache,
# plan_boundary_cache and recovery_activation_cache::plan_schema (including
# planned catalog switch and non-handoff restart). Do not select just their
# names: the other scenarios in that binary remain cache contracts too.
# core_settings_cache is the baseline the integration PR S0 registered; PR 10 adds
# scenarios to it. nextest rejects a binary name that matches nothing, so the
# integration PR that precedes a feature PR registers its new cache binary (for
# PR 15: core_pr_cache, registered by I-14; for PR 17c: core_eval_cache, registered
# by the CLI core-client integration PR; for PR 16a-2: core_renderer_cache,
# registered by integration-16a2) together with a placeholder test file. (No comments inside the filter itself: nextest does not parse them.)
# scripts/tests/test_check_central_files.sh checks that every binary named in
# CACHE_CONTRACT_FILTER has a test target.
# shellcheck disable=SC2034 # Consumed by scripts that source this file.
CACHE_CONTRACT_FILTER='
  package(=mock-openai)
  | (package(=providers) & (
      (kind(lib) & test(observe::cache))
      | (kind(lib) & test(wire::responses::))
      | (kind(lib) & test(responses_replay::))
      | binary(=responses_encrypted_replay)
      | (kind(lib) & test(provider::cursor::tests))
      | (kind(lib) & test(provider::claude::))
      | binary(=cache_prefix_contract)
      | binary(=cache_regression_contract)
      | binary(=codex_cache_regression)
      | binary(=codex_unauthorized_recovery)
      | binary(=codex_responses_lite)
      | binary(=claude_contract)
      | binary(=claude_transport)
      | binary(=claude_attempt_metadata)
      | binary(=claude_account_binding)
      | binary(=request_turn_context)
      | binary(=anthropic_signed_replay)
      | (binary(=anthropic_web_search) & test(search_isolated_from_chat_cache_and_claude_response_correlation))
      | (binary(=image_generation_contract) & test(ac4_image_generation_preserves_chat_cache_baseline))
      | (binary(=codex_web_search) & test(hosted_search_does_not_replace_the_chat_cache_baseline))))
  | (package(=eval) & binary(=structured_retry_cache))
  | (package(=routing) & binary(=claude_identity))
  | (package(=runtime) & (
      binary(=cache_preservation_e2e)
      | (kind(lib) & test(agent_loop::tools_waiting))
      | (kind(lib) & test(quiesce::recovery_cache_tests))
      | (kind(lib) & test(quiesce::recovery_06b_tests::prepared_adoption::models))
      | binary(=claude_cache_e2e)
      | binary(=mcp_cache_e2e)
      | binary(=claude_compaction_cache)
      | binary(=request_turn_context)
      | binary(=benchmark_wire)
      | binary(=structured_escalation)
      | binary(=tool_output_history)
      | binary(=system_prompt_seam)
      | binary(=gpt6_orchestrator_cache)
      | binary(=compaction_triggers)
      | binary(=model_switch_cache)
      | binary(=owned_route_fallback)
      | binary(=builtin_git_skill)
      | binary(=skill_hot_reload_cache)
      | binary(=thread_tools_cache)
      | binary(=pull_request_observer_cache)
      | binary(=pull_request_review_cache) | binary(=pull_request_auto_fix_cache)
      | binary(=harness_notice_delivery_cache)
      | binary(=pull_request_e2e)))
  | (package(=evorch-core) & (binary(=core_cache) | binary(=core_mcp_cache) | binary(=core_workspace_cache) | binary(=startup_recovery) | binary(=core_settings_cache) | binary(=core_pr_cache) | binary(=core_eval_cache) | binary(=core_renderer_cache)))
  | (package(=tools) & binary(=bounded_output))
  | (package(=gui) & (binary(=artifact_pages_cache) | binary(=pull_requests_cache) | binary(=pull_request_phase2_gui_cache) | binary(=harness_notice_recovery)
      | (binary(=lane_delivery_host) & test(attention_query_and_display))))
'
IO_CONTRACT_FILTER='
  (package(=storage) & (binary(=lane_delivery) | binary(=conversation_plan) | binary(=pull_request_auto_fix) | binary(=io_contracts) | binary(=retention_io_contracts) | binary(=archive_index_io_contracts) | binary(=content_blob_io_contracts) | binary(=run_messages_io_contracts) | binary(=event_body_io_contracts) | (kind(lib) & (test(maintenance::convert_bodies::tests::one_run_writes_a_log) | test(maintenance::convert_bodies::tests::one_map_writes_a_log)))))
  | (package(=runtime) & kind(lib) & test(ownership::tests))
  | (package(=runtime) & kind(lib) & test(conversation_plan::tests))
  | (package(=workbench-host) & (binary(=conversation_plan_reads) | (kind(lib) & test(areas::execution::plan::tests))))
  | (package(=runtime) & kind(lib) & test(lane_delivery::tests::harness::))
  | (package(=runtime) & binary(=pull_request_log_source_io))
  | (package(=gui) & (
      (kind(lib) & test(storage_bridge))
      | binary(=evorch-gui)
      | binary(=storage_bridge)
      | binary(=diagnostic_ledger)
      | binary(=ownership_close_headless)))
'
