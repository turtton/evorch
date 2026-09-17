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

## Re-review 1
- Blockers resolved: durable state, budget circuit breaker, provider admission, typed reviewer evidence.
- Remaining blocker: GUI `demo_loop` fixture returns legacy approval without complete evidence, so the strengthened gate correctly requests repair and the scripted demo times out.
- Required next repair: update `crates/gui/src/model/demo.rs` reviewer fixture to submit typed complete evidence and rerun workspace locked tests.

## Blocker #3 minimal retry — RED not reproducible

- Inspected only review.rs, supervisor finish_review, and review_loop.rs.
- Current ReviewLoop already rejects an empty checklist and calls criterion_verified for each criterion; finish_review already obtains reviewer_result and passes it to parse_reviewer_output before the prose fallback.
- Added two regression tests without editing production. Each command below passed (1 passed, 0 failed):
  - `cargo test -p runtime --test review_loop approve_with_missing_evidence_requests_update -- --exact`
  - `cargo test -p runtime --test review_loop approve_with_empty_checklist_requests_update -- --exact`
  - `cargo test -p runtime --test review_loop supervisor_prefers_typed_tool_result_over_prose -- --exact` (existing T11-path test).
- No RED evidence exists for these requested cases in the current worktree. Stopped rather than weakening assertions or reverting an existing fix to manufacture RED. No production edits, commit, or push. Two test additions remain uncommitted. Full evidence-validator semantics and remaining quality gates are not claimed verified by this minimal probe.

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

## Blocker #3 acceptance repair — 2026-09-18

- Scope: ReviewLoop evidence validation and review_loop regression tests only;
  existing typed-first transport, prose fallback and bounded rounds are preserved.
  CriterionEvidence already supports every required field; no event-bus change.
- Initial worktree already contained staged review recovery/shared gate changes
  and empty-checklist/missing-evidence tests. Those tests already passed. The
  shared predicate required all three references, contrary to this repair's
  at-least-one contract. ReviewLoop now validates that contract locally without
  changing the finish gate or including unrelated staged changes in this commit.
- RED before production edit: `cargo test -p runtime --test review_loop` exited
  101: **10 passed, 1 failed**. New
  `approval_requires_valid_evidence_with_at_least_one_reference` failed for
  command `test`, exit 0, current HEAD_A, references `[Some("diff"), None, None]`:
  actual merge permission false, expected true.
- GREEN: same command exited 0: **11 passed, 0 failed**. Cases cover each single
  reference, empty/whitespace command, nonzero exit, stale SHA, absent/blank
  references, absent evidence and empty checklist. Existing contradictory-prose
  typed-first, prose fallback, repeated findings and bounded-round tests pass.
- `cargo check -p runtime`: exit 0.
- `cargo fmt --check`: exit 0.
- `cargo clippy -p runtime --tests -- -D warnings`: exit 0.
- LSP diagnostics: no errors in review.rs or review_loop.rs.
- Self-review: validation stays in the typed review boundary with exhaustive
  status matching; no new helper, API, dependency, unsafe, production unwrap,
  logging, or extra parameter. Invalid approval records RequestUpdate with the
  affected criterion IDs; empty checklist records its own concrete finding.
- Existing supervisor/recovery and other staged edits are excluded; no push.

## Blockers #2 provider admission / #4 title fixture — 2026-09-18

- RED first: `cargo test -p runtime --test provider_admission` failed all 3 rejection tests with `failed admission registered a worker`. `cargo test -p gui --test auto_title_provider` failed on GET Content-Length and wrong title. Oversized-catalog RED also failed before bounding the response.
- Registration boundary now waits for selected route and configured fallback model advertisement/connectivity verification before reserving worker slots, enqueueing tasks, inserting runs, or emitting runtime start events. Synchronous API returns a reserved ID; `wait` returns admission failure without a registered worker. Direct complete verification remains a compatibility path.
- Catalog requests: ten-second timeout and 1 MiB body bound. Reused mock-openai `spawn_with_models`. GUI fixture serves `/v1/models`, then completion, asserting quick=fast and explicit=chosen titles.
- GREEN: provider_admission 4, provider_verify 6, list_models_contract 5, codex_models_contract 6, auto_title_provider 1. Full providers tests pass with `--test-threads=1`; runtime library 366 pass. Actual runtime HTTP/SSE happy path returns `admitted` after catalog discovery.
- `cargo check -p runtime -p providers -p gui --all-targets` and matching clippy `-- -D warnings`: pass. All 12 touched Rust paths: LSP clean and scoped rustfmt check pass.
- Wider gates: runtime background 2 failures are fixed-model event-count assertions; GUI demo_loop fails on missing review evidence and demo repair shell argument splitting. Package fmt reported concurrent review_loop formatting. These are outside this repair; budget tracker/review transport were not edited.
- Detailed evidence: `.omo/notepads/v07-durable-execution-substrate/reviewer-gate-provider-admission.md`. Shared oversized runtime/compose hosts receive seam wiring only; new admission module is 55 pure LOC. No push.

## Demo reviewer fixture repair — 2026-09-18

- RED first: `cargo test -p gui --test demo_loop demo_goal_reaches_awaiting_merge_then_complete_deterministically --locked` exited 101 after 21.01s. The demo timed out at `crates/gui/src/model/demo.rs:318` (`demo script gate timed out after 10s`); the collected events showed the reviewer recorded an approved criterion with `evidence: None`, then produced `request-update` and exhausted the scripted repair path before the goal reached completion.
- Repair: the approval script now supplies complete evidence for `ac-1`: `cargo test --workspace --locked`, exit status `0`, the fixture's current 40-character HEAD_B SHA, non-empty `main...evorch/task/run-2` diff ref, artifact path, and RED evidence path. The test collector ignores volatile `TaskProgressed` events (timestamps and temporary worktree paths) when comparing the lifecycle sequence.
- GREEN: `cargo test -p gui --test demo_loop --locked` exited 0: **1 passed, 0 failed** in 2.02s. Both deterministic runs reached `complete` and merge approval remained evidence-gated.
- Requested gates: `cargo fmt --check`, `cargo check --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, and `git diff --check` each exited 0. `cargo test --workspace --locked` exited 101 only at the pre-existing `runtime_wiring::live::production_non_demo_composition_uses_loaded_config_and_file_store` assertion (`left: 2`, `right: 1`); all other reported workspace tests, including `demo_loop`, passed.

## GUI runtime wiring fixture repair — RED — 2026-09-18

- Exact test before editing: `cargo test -p gui --test runtime_wiring --test runtime_wiring live::production_non_demo_composition_uses_loaded_config_and_file_store`
- Result: exit 101, 0 passed, 1 failed. `crates/gui/tests/runtime_wiring/live.rs:88` observed `left: 2`, expected `right: 1`; provider admission adds the `/v1/models` preflight alongside the completion request.

- Repair: `crates/gui/tests/runtime_wiring/live.rs` now filters `RecordedRequest` values to
  `/v1/chat/completions`, asserts exactly one completion request, and retains the authorization
  and selected-model assertions against that request. No runtime or provider code changed.
- GREEN: `cargo test -p gui --test runtime_wiring` exited 0: **3 passed, 0 failed**.
- Requested gates: `cargo fmt --check`, `cargo check --workspace`,
  `cargo clippy --workspace --all-targets -- -D warnings`, and `git diff --check` each exited 0.
- `cargo test --workspace --locked` exited 101 only in pre-existing
  `crates/runtime/tests/background.rs`: `background_start_is_observable_before_wait_and_completion_is_success_only`
  and `cancel_mid_model_turn_emits_cancelled_and_error` failed; the GUI runtime wiring tests passed.
