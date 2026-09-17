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

## Reviewer Gate blocker #3 repair (2026-09-18)

- RED before production changes: `cargo test -p runtime --test review_evidence_contract` failed all 8 cases (empty checklist, absent evidence, blank command, nonzero exit, mismatched SHA, absent diff, blank artifact, absent RED). Every invalid approval incorrectly returned `Approve`.
- Transport RED: `cargo test -p runtime --test reviewer_transport` failed with missing `reviewer_result` API after correcting a test-only import. `cargo test -p runtime --test orchestrator_loop_e2e goal_runs_to_awaiting_merge_then_complete_with_one_request_update_round -- --exact` failed with approval timeout; snapshot recorded `contradictory prose` rather than the tool's approval.
- Recovery RED: `cargo test -p runtime --test review_recovery` failed because restore API was absent; later `exhausted_restored_review_cannot_approve -- --exact` failed because an exhausted restored loop could approve.
- `submit_review` is a reviewer-only meta operation. Its input is parsed into `ReviewResult` at dispatch and stored per RunId independently of `run_result` (which remains final text). Supervisor consumes that channel first, with raw/fenced text as compatibility fallback. The real supervisor E2E submits typed approval then contradictory request-update prose and reaches merge approval/complete.
- ReviewLoop and finish gate share evidence validation: non-empty checklist, Met status, non-blank command, exit 0, matching non-empty head, non-blank diff/artifact/RED references. Missing evidence stays readable for legacy payloads but cannot approve. Review state adopts persisted round count and latest request-update findings; exhausted restored loops cannot approve.
- GREEN: `cargo test -p runtime --test review_evidence_contract --test review_loop --test reviewer_transport --test review_recovery --test finish_gate_table --test orchestrator_loop_e2e`: 43 passed (8/8/1/3/19/4).
- Recovery regressions: `cargo test -p runtime --test goal_recovery --test goal_persistence --test goal_state_machine --test review_recovery --test finish_gate_table`: 39 passed. `cargo test -p agents`: 27 passed. `cargo check -p runtime`: pass. Targeted rustfmt check of all 18 touched Rust paths: pass; LSP on all 18: no diagnostics.
- Broader checks are not wholly green: runtime lib 365 passed / 1 failed (`compose::tests::category::delegated_worker_uses_category_model_when_quick_is_bound`, provider advertisement); background 3 passed / 2 failed (event-count assumptions in start/cancel). All-target clippy stopped at concurrently added `runtime.rs` admissions field `clippy::type_complexity`. Package fmt initially also reported concurrent budget/durable formatting. These unrelated files/hunks were not repaired or staged here.
- Size/self-review: review.rs stays below 250 pure LOC (warning band); new channel/tests are small. Existing oversized runtime/supervisor and E2E fixture receive only seam wiring or scenario changes; full splitting is deferred because those hosts contain concurrent provider/budget/durable work. No new untyped interior values, unsafe, casts, unchecked production unwraps, dependencies or logging. Prose instructions are not pinned by assertions.
- Commit scope is blocker #3 only, including shared-file hunks staged separately. No push.
