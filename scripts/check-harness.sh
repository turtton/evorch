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
run runtime cargo test -p runtime --test restore_expansion --test restore_tool_intent \
    --test shell_jobs_integration --test state_transitions --test background --test messaging_loop \
    --test workspace_cleanup \
    --test wait_runs --test user_questions --test escalation_questions --test escalation_handoff \
    --test context_observability --test compaction_engine --test compaction_policy \
    --test compaction_triggers --test compaction_continuation --test meta_ops --lib
run providers cargo test -p providers --lib
run tools cargo test -p tools -p sandbox
run storage cargo test -p storage --test user_questions --test migration
run gui cargo test -p gui --test user_questions_headless --test goal_restore_authority \
    --test goal_restore_binding --test runtime_wiring --lib
if [[ "$mode" == full ]]; then
    run workspace cargo test --workspace --no-fail-fast
    run lint cargo clippy --workspace --all-targets -- -D warnings
    run browser cargo test -p gui --features browser
    run otel cargo test -p event-bus --features otel-exporter
fi
printf 'Passed. Logs are temporary files in %s; remove this directory when no longer needed.\n' "$out"
