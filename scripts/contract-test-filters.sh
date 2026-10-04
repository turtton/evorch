#!/usr/bin/env bash
# Keep the offline gates and the workspace remainder disjoint. nextest isolates
# each test process, including providers' process-global cache tests.
# shellcheck disable=SC2034 # Consumed by scripts that source this file.
CACHE_CONTRACT_FILTER='
  package(=mock-openai)
  | (package(=providers) & (
      (kind(lib) & test(observe::cache))
      | binary(=cache_prefix_contract)
      | binary(=cache_regression_contract)
      | binary(=codex_cache_regression)))
  | (package(=runtime) & (
      binary(=cache_preservation_e2e)
      | binary(=benchmark_wire)
      | binary(=structured_escalation)
      | binary(=tool_output_history)
      | binary(=system_prompt_seam)
      | binary(=compaction_triggers)
      | binary(=builtin_git_skill)
      | binary(=skill_hot_reload_cache)))
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
BROWSER_TEST_FILTER='
  (kind(lib) & test(/^browser::/))
  | binary(=browser)
  | binary(=evorch-gui)
'
