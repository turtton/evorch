#!/usr/bin/env bash
# Deterministic harness regressions; no provider credentials or external model calls.
set -euo pipefail
cd "$(dirname "$0")/.."
mode=${1:-focused}
case "$mode" in focused|full) ;; *) printf 'Usage: %s [focused|full]\n' "$0" >&2; exit 2;; esac
out=$(mktemp -d /var/tmp/evorch-harness-check.XXXXXXXX)
printf 'Harness logs: %s\n' "$out"
run() {
    local name=$1
    shift
    printf 'Running %s\n' "$name"
    if ! "$@" >"$out/$name.log" 2>&1; then
        tail -n 80 "$out/$name.log" >&2
        printf 'Full log: %s/%s.log\n' "$out" "$name" >&2
        return 1
    fi
}
run format cargo fmt --all --check
run cache-contracts scripts/check-cache-contracts.sh
# The grouped targets run every test of their group; the filters keep the former per-file selection.
run runtime cargo test -p runtime --test restore_state --test tools_shell --test delegation \
    --test waits_messaging --test escalation_ownership --test compaction_budget -- \
    restore_expansion:: restore_tool_intent:: shell_jobs_integration:: state_transitions:: \
    background:: messaging_loop:: wait_runs:: user_questions:: escalation_questions:: \
    escalation_handoff:: context_observability:: compaction_engine:: compaction_policy:: \
    compaction_continuation:: meta_ops::
run runtime-standalone cargo test -p runtime --test compaction_triggers --lib
run providers cargo test -p providers --lib
run tools cargo test -p tools -p sandbox
run storage cargo test -p storage --test run_data_suite --test platform_suite -- \
    user_questions:: migration::
run gui cargo test -p gui --test runtime_threads -- \
    user_questions_headless:: goal_restore_authority:: runtime_wiring::
run gui-lib cargo test -p gui --lib
if [[ "$mode" == full ]]; then
    run workspace cargo test --workspace --no-fail-fast
    run lint cargo clippy --workspace --all-targets -- -D warnings
    run gui-requires-cef scripts/tests/test_gui_requires_cef.sh
    run otel cargo test -p event-bus --features otel-exporter
fi
printf 'Passed. Logs are temporary files in %s; remove this directory when no longer needed.\n' "$out"
