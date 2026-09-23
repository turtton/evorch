#!/usr/bin/env bash
# Offline cache correctness gate. No credentials, external API, or paid model calls.
# Run the same contract before pushing and in CI; keep deliberate compaction covered.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo test -p mock-openai
cargo test -p providers --lib observe::cache
cargo test -p providers --test cache_prefix_contract --test cache_regression_contract --test codex_cache_regression
cargo test -p runtime --test cache_preservation_e2e --test tool_output_history --test system_prompt_seam --test compaction_triggers
cargo test -p tools --test bounded_output
