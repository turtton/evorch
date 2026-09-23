# Harness reliability validation (2026-09-23)

The implementation includes `main` commit `6f10ec1` (Tasks/Agents UI unification).
The operator explicitly approved the ADR 0027 revision and finalization correction
on 2026-09-23, and authorized merging to main, pushing, and verifying CI.

## Integration results

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
