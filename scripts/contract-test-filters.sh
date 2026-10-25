#!/usr/bin/env bash
# Keep the offline gates and the workspace remainder disjoint. nextest isolates
# each test process, including providers' process-global cache tests.
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
      | binary(=claude_contract)
      | binary(=claude_transport)
      | binary(=claude_attempt_metadata)
      | binary(=claude_account_binding)
      | binary(=request_turn_context)
      | binary(=anthropic_signed_replay)
      | (binary(=anthropic_web_search) & test(search_isolated_from_chat_cache_and_claude_response_correlation))
      | (binary(=image_generation_contract) & test(ac4_image_generation_preserves_chat_cache_baseline))
      | (binary(=codex_web_search) & test(hosted_search_does_not_replace_the_chat_cache_baseline))))
  | (package(=routing) & binary(=claude_identity))
  | (package(=runtime) & (
      binary(=cache_preservation_e2e)
      | binary(=claude_cache_e2e)
      | binary(=claude_compaction_cache)
      | binary(=request_turn_context)
      | binary(=benchmark_wire)
      | binary(=structured_escalation)
      | binary(=tool_output_history)
      | binary(=system_prompt_seam)
      | binary(=compaction_triggers)
      | binary(=model_switch_cache)
      | binary(=owned_route_fallback)
      | binary(=builtin_git_skill)
      | binary(=skill_hot_reload_cache)
      | binary(=thread_tools_cache)))
  | (package(=tools) & binary(=bounded_output))
'
IO_CONTRACT_FILTER='
  (package(=storage) & binary(=io_contracts))
  | (package(=runtime) & kind(lib) & test(ownership::tests))
  | (package(=gui) & (
      (kind(lib) & test(storage_bridge))
      | binary(=evorch-gui)
      | binary(=storage_bridge)
      | binary(=diagnostic_ledger)
      | binary(=ownership_close_headless)))
'
