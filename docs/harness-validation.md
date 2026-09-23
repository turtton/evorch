# Harness reliability validation (2026-09-23)

The implementation includes `main` commit `6f10ec1` (Tasks/Agents UI unification).
The operator explicitly approved the ADR 0027 revision and finalization correction
on 2026-09-23, and authorized merging to main, pushing, and verifying CI.

## Integration results at `e2287e7`

- `cargo test --workspace --no-fail-fast`: **3,439 passed, 0 failed, 48 ignored**.
- `cargo clippy --workspace --all-targets -- -D warnings`: passed.
- GUI Clippy with all targets, `browser`, and `-D warnings`: passed.
- Event-bus Clippy with all targets, `otel-exporter`, and `-D warnings`: passed.
- `cargo fmt --all --check`, `git diff --check`, script shell syntax: passed.

These results include the final lifecycle, root-renewal and tracing test corrections.
Targeted checks also completed:

- Finalization, restore, cancellation and escalation integration: 98 passed;
  runtime/tools unit tests: 553 passed.
- All root history renewal checks, including active and admission-pending children:
  47 integration and 18 related unit tests passed.
- Compaction continuation and shared fixture consumers: 22 passed. The fixture
  probes the actual Explorer prompt/schema and uses serialized UTF-8 bytes rather
  than a schema-free fixed allowance. Compaction still fires once at the intended
  boundary and preserves agent messages/tool pairs.
- Agents UI after integrating main: 12 passed.
- Escalation question delivery and existing escalation/question regressions:
  18 passed.
- Event-bus tests with `otel-exporter`: 170 passed.
- Cancellation notification ordering: 33 related tests passed; the gated-drain
  regression proves that cancellation is withheld during teardown and delivered
  before the terminal Error event.
- Providers suite: 315 passed. Its 161 unit tests passed 100 runs at 32-way test
  parallelism. The tracing test keeps a second dispatcher alive and forces another
  thread to register the shared callsite first. Request-ID coverage now verifies
  monotonicity and uniqueness under concurrent allocation instead of incorrectly
  requiring adjacent values from a globally shared counter.

The 48 ignored workspace tests require environments such as bwrap, real rendering
or Chromium. They are not counted as passing. CI separately exercises its configured
rendering, browser and Nix environments. Local tests call no external model.

## Finalization and recovery coverage

- Terminal visibility follows shell drain, final snapshot and workspace cleanup
  or handoff preparation. The same-ID continuation regression deliberately blocks
  drain and proves that wait cannot return before the old incarnation is finished.
- Thirty-five consecutive cancelled shell runs do not exhaust the 32-handle limit;
  releasing handles preserves durable uncertainty markers and recovery refusal.
- Failed final persistence keeps unobserved handles and the prior checkpoint.
  Snapshot failure remains non-fatal when no uncertain shell effects are present.
- Escalation retains source-Done-before-new-root-Started ordering and prepares
  question inheritance before startup. Failed inheritance prevents a new root.
- Root renewal checks current authority, stale ownership, mismatched projects,
  completed boards, retained claims, active/admission-pending descendants, and
  disk-only delivery. Refusal preserves the last checkpoint.
- Tool intent is persisted before dispatch. Incomplete or uncertain side effects
  are never blindly replayed.

## Interaction and diagnostic coverage

- Shell start/yield cancellation, reaping before snapshot unlock, read availability,
  conflicting write rejection, unobserved results, stop/finish, pipe and PTY capture.
- Durable questions across restart, free text, first-answer idempotence, finite
  limits, explicit consumer links, run-ID reservation, late-answer completion race,
  failed reads, stale owners including admission, and old source event fencing.
- GUI conversation scoping, explicit answer submission, durable acknowledgement,
  ownership handoff during answer, history/live binding, diagnostic invalidation,
  and restored context without reviving running timers.

Reproduce with [the harness script](../scripts/check-harness.sh). Implementation
limits, including PTY line buffering, are in [the runtime notes](harness-runtime-improvements.md).

## CI follow-up: bounded GUI fixtures

The first main CI run, [35809013943](https://github.com/turtton/evorch/actions/runs/35809013943),
passed Nix build/checkPhase, offscreen rendering, Chromium E2E, workspace tests,
lint, otel-exporter tests and schema drift checks. The operator cancelled it after
the GUI browser-feature suite stalled in `runtime_wiring`.

The test model subscribed to a 16-event queue before startup and only read it on
its final turn. Startup/progress could overflow that queue; `recv().expect()` then
panicked on `Lagged`, leaving the parent completion wait pending. The fixture now
recovers from lag, returns a model error on closure/deadline, and bounds both run
completion and GUI convergence waits. A deterministic overflow regression covers
this case without changing production behavior or weakening task-row assertions.

Parallel reproduction also exposed a separate OAuth fixture collision: separate
workspace/browser test processes competed for the production callback ports. The
fixture now binds an ephemeral callback port, and both mock issuer accept loops
have deadlines. Production OAuth ports are unchanged.

Validation after these test-only changes:

- GUI browser-feature suite: **973 passed, 0 failed, 24 ignored**.
- `runtime_wiring`: all four tests passed 30 times each with 1, 2 and 4 Tokio
  workers (90 suite runs); default-feature execution also passed all four tests.
- OAuth failure-path fixture: eight concurrent processes all passed. Before the
  fix, the same reproduction left six processes waiting indefinitely.
- GUI browser-feature Clippy with all targets and `-D warnings`: passed.
- Format, diff and harness-script syntax checks: passed.

The harness script now includes `runtime_wiring` in focused checks and executes
the GUI browser-feature tests in full mode instead of only checking compilation.
The full workspace figures above are from `e2287e7`, before this fixture follow-up;
the next main CI run validates the complete tree again.

## Prompt-cache regression gate

Run `scripts/check-cache-contracts.sh` for the offline cache contract. The same
script runs in the Lefthook pre-push hook, the harness checks, and a named CI step.
Hooks must be installed locally; CI provides the shared check. Repository branch
protection is separate from this change.

`StreamingMockOpenAi::spawn_with_prompt_cache` derives cached usage from the
actual HTTP requests, instead of returning scripted high cache counts. It puts
stable request settings before the conversation, canonicalizes JSON object keys,
keeps array order and string bytes, and finds the longest prior input prefix for
the same model and cache key. One UTF-8 byte is one synthetic token; this is a
mutation detector, not a real tokenizer, cache TTL/routing simulator, or billing
estimate. JSON and SSE return consistent input/cached/total usage.

The runtime E2E exercises configuration, routing, provider HTTP/SSE, tool returns,
compaction events, and usage aggregation. Twelve consecutive tool calls cross
the former eight-result pruning boundary, including large output artifacts,
errors, and multilingual text. Every ordinary turn must retain the full previous
input prefix and report at least that much cached input. Raw request assertions
use a separate test oracle, independent of both the mock cache calculation and
the production warning logic. Actual provider clients also verify stable wire
input for OpenAI, OpenAI Compatible, Codex Responses, and Anthropic, including
images, multiple tool results, and changing tool registration order.

A successful same-run `Compacted` event permits replacement only in the next
request, with the matching new checkpoint and unchanged instructions/settings.
Subsequent requests must preserve that new prefix. Both manual and automatic
compaction are covered; checkpoint-like text alone grants no exemption.

When adding a cache-affecting feature, extend the relevant E2E path and add a
negative case for its unintended history mutation. Intentional boundaries need
an explicit event and coverage showing reuse resumes afterward. Passing these
contracts protects the exercised request construction paths; it does not promise
production cache availability or automatically cover every future execution path.

Local validation on 2026-09-23: the cache gate passed all 82 tests in 7.4 seconds
with existing build artifacts (runtime E2E: three tests in 2.5 seconds). Temporarily
reintroducing count-based rewriting after eight tool results made the ordinary
E2E fail at request 9 with `previous conversation item 3 changed`. Production
source was restored exactly, and the full gate then passed. Clean compilation
adds normal Rust build time; these figures measure this local checkout, not CI.
