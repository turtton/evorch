# Reviewer Gate — v07-durable-execution-substrate

## Initial review
- Verdict: BLOCKED
- Blockers: 4
- Notes: 5
- Reviewer: plan agent ses_f4e8f7aceffeh2eZP3SwIbY0rT

## Criterion-cited blockers
1. Durable task state: normal execution hard-codes cursor/artifact/failure fields to null; delayed progress can reopen terminal state.
2. Budget/provider admission: budget publication emits only diagnostics; provider verification occurs after run registration/start and does not validate selected model.
3. Typed review gate: supervisor consumes final text, typed evidence is optional, and approvals do not require complete evidence.
4. Regression: gui auto_title_provider fails because its mock does not handle the new `/v1/models` preflight request.

## Required repair evidence
- RED then GREEN output for each blocker-specific regression test.
- Full workspace gates after repairs.
- Re-review verdict and any remaining notes before PR creation.

## Blocker #1 repair — 2026-09-18

- Production path: a real `AgentRuntime` worker produces `validated patch`, then
  receives a Length failure containing `truncated invalid patch`. No passing
  fixture injects TaskProgressed cursor/artifact/failure fields. Captured bus
  events are appended to SQLite, reconciled, closed, reopened and reconciled;
  GoalLedger replay is adopted by a fresh supervisor, then `resume_task("task")`
  dispatches a new generation. The model boundary receives the persisted typed
  continuation, including visible-message cursor, latest valid artifact and
  exact `model response reached length limit` reason.
- RED (before production edits): `cargo test -p runtime --test durable_worker_boundary`
  exited 101: **0 passed, 5 failed**. Failure reason was `run failed`; completed
  and cancelled tasks reopened as running after delayed same-generation progress;
  delayed heartbeats mutated terminal state.
- Additional RED: `cargo test -p runtime --test durable_worker_boundary real_worker_failure`
  exited 101: **0 passed, 1 failed**, artifact was `truncated invalid patch`
  instead of `validated patch`. Terminal failure publication now preserves the
  previously valid artifact.
- GREEN: `cargo test -p runtime --test durable_worker_boundary --test goal_supervisor_continuation --test goal_persistence`
  exited 0: **5 + 13 + 3 passed**. The continuation suite includes the concurrently
  added budget test. Terminal tests compare full payloads in live ledger,
  SQLite projection and replay, for Completed/Cancelled × progress/heartbeat.
  Operator cancellation persists exact `cancelled by operator`.
- Storage: `cargo test -p storage` exited 0 (all unit/integration/doc tests passed).
- `cargo check -p runtime -p storage --all-targets`: exit 0.
- LSP: all nine changed Rust files report no diagnostics. Scoped rustfmt check
  passes. New production module owns only durable execution publication; existing
  TaskContinuation/TaskStatus and public APIs are preserved, with no new dependency,
  unsafe, lint allowance, narrowing cast or production unwrap/expect.
- Shared-worktree gates: `cargo clippy -p runtime -p storage --all-targets -- -D warnings`
  was blocked by the concurrent provider-admission change's `type_complexity` at
  `crates/runtime/src/runtime.rs:54` (not part of blocker #1). Workspace fmt check
  was blocked by concurrent edits in `gui/tests/auto_title_provider.rs` and
  `runtime/src/compose/verification.rs`; all blocker #1 paths pass scoped fmt.
  These unrelated changes are not included in this repair commit.
