# Reviewer Gate blocker #2 — budget portion (#118)

## RED before production edits

- `cargo test -p runtime --test budget_checkpoint budget_overrun_emits_budget_exhausted_once -- --exact`: failed, actual tool IDs `[0,1,2,3,4,5,6,7]`, expected `[0,1,2,3,4]`.
- `cargo test -p runtime --test goal_supervisor_continuation budget_diagnostic_blocks_once_without_terminal_continuation`: failed, goal remained Active instead of Blocked.

## Repair and evidence

- BudgetCounters returns typed Continue/Exhausted, latches the first breach and emits one typed DiagnosticEvent. Tool capacity is exhausted at equality; token/reread/no-progress/elapsed retain strict over-limit semantics.
- Agent turn, post-usage, post-round, pre-provider and tool admission/dispatch boundaries stop on Exhausted. Batch segments are bounded by remaining tool capacity, including shared waves: calls six through eight never execute under a five-call budget. No provider call six is admitted.
- Existing max_elapsed setting is checked at boundaries; no new setting or independent watchdog. Already-running work is not preempted solely by elapsed time.
- Run terminates Error with BudgetExhausted/NoProgress plus exact breach detail; supervisor blocks the goal once so terminal events cannot launch continuation work. Warning-only escalation remains advisory. Checkpoints at 50/100/150/200/250 remain intact with independent progress budgets raised in their fixtures.
- Durable reason/cursor use the concurrent worker's single `transition -> publish_durable_task` path, not a second TaskProgressed producer. Regression compares exact diagnostic-derived failure reason and deserializes saved messages to verify precisely five tool results. This commit depends on the concurrent durable-worker boundary repair for persistence.
- Targeted GREEN: `cargo test -p runtime --test budget_checkpoint --test goal_supervisor_continuation`: 8 + 13 = 21 passed. These execute real AgentRuntime, standard read tools, DirectSandbox, and supervisor actor.
- `cargo test -p runtime --lib budget_tracker`: 2 passed (progress reset and saturating usage).
- `cargo check -p runtime`: pass. `cargo fmt -p runtime -- --check`: pass. LSP: all ten touched Rust paths clean (absolute paths required; two cancelled requests rerun successfully).
- Strict targeted clippy was attempted: `cargo clippy -p runtime --lib --test budget_checkpoint --test goal_supervisor_continuation -- -D warnings`; blocked by concurrent provider-admission `runtime.rs:54` type_complexity. Provider admission files deliberately untouched.
- Size: budget tracker 186, loop budget 38, supervisor budget 32, fixture 72, supervisor regression 52 pure LOC; budget integration tests approximately 239 (warning band). Existing large agent/tool/supervisor state-machine files receive boundary wiring only, avoiding concurrent ownership conflicts.
- Scope: budget-only repair. No push. Other agents' changes must not be included in the budget commit.
