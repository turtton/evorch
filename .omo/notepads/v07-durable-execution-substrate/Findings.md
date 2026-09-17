# W3-T6 / issue #118

## RED

- Command: `cargo test -p runtime --test goal_supervisor_continuation stale_worker_transitions_to_retrying_with_fresh_run_id -- --exact`
- Result: 1 failed / 0 passed, `stale worker must be retried by the sampler: Elapsed(())` (3.01s).
- Production code unchanged at RED. Attached running task carries heartbeat `1`, attempt `1`; existing sampler does not schedule retry.
- Existing cadence: supervisor `stall_tick`, interval `max(stall_check_secs, 1)` seconds, missed ticks skipped. Reuse it; no new actor/watchdog.

## Implementation / GREEN

- Worker loop emits heartbeat at existing `publish_budget` turn/tool boundaries. TaskProgressed heartbeat updates only heartbeat in an existing continuation; the first heartbeat seeds its input. Ledger replay and SQLite projection preserve cursor, status, attempts and failure reason.
- `orchestration.stale_ttl_secs` defaults to 1800; detection uses strictly greater than TTL. Existing actor sampler defaults to 30 seconds (minimum 1 second), with an immediate first interval tick and Skip missed-tick policy. There is no separate watchdog. Detection can be delayed by actor work/scheduling; long tools/providers with no boundary may be considered stale.
- Only active, attached, current-generation worker tasks with an explicit heartbeat are sampled. Detached/paused/blocked/terminal goals and tasks without a heartbeat are not automatically retried.
- Stale mark precedes failed/stale-worker progress and the existing bounded continuation path. Retry preserves task identity, input, cursor, artifact and parent; increments attempt and reserves a fresh run. Replacement remains retrying until terminal outcome; heartbeat does not reset it to running.
- Regression test exercises the real actor timer, replacement agent/model boundary, SQLite reconcile/reopen, and fresh heartbeat. Subscription starts before worker creation to avoid missing the immediate sampler tick.
- Final related runtime tests: 42 passed (continuation 12, recovery 1, persistence 3, state 13, budget 7, stall 6).
- Storage library tests: 87 passed. Config library tests: 64 passed.
- `cargo check -p runtime`: pass. Targeted runtime lib + the six integration test targets clippy with `-D warnings`: pass. `cargo fmt -p runtime -p storage -p config -- --check`: pass. All 12 touched Rust paths: LSP no diagnostics.
- Full `cargo clippy -p runtime --all-targets -- -D warnings` is blocked by pre-existing `tests/interview.rs:109`: TaskRecord initializer lacks the T1 durable fields (E0063). Not changed here.
- Size review: new stale module 85 pure LOC, regression support 106, ledger/durable 231 and supervisor/tasks 237 (warning band). Existing oversized host modules (strict, ledger, supervisor, integration test harness) receive only wiring/settings/test entry additions; broad splitting is deferred to avoid crossing concurrent worker scope.
- Concurrent GUI/workspace-ui and typed-review changes are not included in this commit. No push.
