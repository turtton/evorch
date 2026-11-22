# D-3a2 B3-09 performance check (runner, fixtures, aggregator)

**Scope.** After a dry run showed that the original gate (CPU-limited scope, stress-ng, 3
rounds × 200 pairs, p50/p95 deltas of 2/5 ms) could not run: a round would not fit the 600 s scope
and the A-versus-A noise was hundreds of milliseconds, the user reduced B3-09 to an
**order-of-magnitude check** (question-41eda9b4579b24b09e86a96a7e751928). This is not a latency
gate and it does not support a claim about milliseconds. Its two checks are the ones below.

Nothing here is on a product path. The runner is a cargo example, the aggregator lives in
`shared-test-support/`, and no product flag switches the guard off: arm A is a different sealed
build, not a toggled one.

| File | Role |
|---|---|
| `perf-gate/spec.json` | the frozen counts, workloads and limits (committed before the measurement) |
| `examples/perf_gate/{main,arm,provider,fixture,cells,conditions}.rs` | runner |
| `shared-test-support/perf_gate/mod.rs` | aggregator and conditions check |
| `tests/perf_gate_aggregate.rs` | negative tests (run in CI, no timing) |
| `scripts/perf-gate.sh` | driver: `conditions`, `run`, `aggregate`, `smoke` |

## What is measured

One round, on an otherwise idle fixed host. No cgroup scope, no stress-ng, no C arm. The host
state (load average, top CPU users, kernel, CPU, governor, memory) is recorded in conditions.json
and the load average again in the observations file.

- **Arms.** A = `b420a684` (the parent of B1 `d39d2074`, #720: guard not yet introduced). B\* = A
  plus the ten B commits cherry-picked in order (`d39d2074 66848cb6 b846cc81 6e60d280 3769675d
  2cdc33f1 656be97f ff03eb9d 956d2ffe 908fe00f`). Both are sealed release cores built by
  `scripts/product-artifact.sh build` with the same toolchain, flags and features; the SHA-256
  of each (and of B\*'s helper and bundle) is in conditions.json. B\* isolates B from the
  517-file unrelated drift between A and main; it is not a main SHA.
- **Core state (normal API only).** 20 idle threads × 50 messages × 8 KiB (25 turns of one padded
  8 KiB reply each), a repository of 1000 files × 16 KiB with a fixed commit id, two active
  mock-backed agent threads that send a `read` turn every 100 ms. Threads rotate every 20 turns
  in both arms (conditions.json `thread_rotation`).
- **Shell RTT.** From the instant the provider flushed the `shell` tool call to the instant the
  request carrying the tool result arrived, on the runner's monotonic clock. It covers the core's
  dispatch, the real `sh -c`, output handling and the way back. Workloads: `grep -F -- D-7
  fixture.txt`, `git diff --quiet -- tracked.txt`, `:`. A sample whose result is not
  `exit_code: 0` (and, for grep, lacks the match) is an error, not a timing.
  5 warm-up probes per arm, then 50 pairs per workload, always A then B (ABAB).
- **Decision.** Per workload: `p50(B*) <= 1.5 * p50(A)` (integer arithmetic, nearest-rank).
  The per-pair differences (p50, p95) and pXX(B) − pXX(A) are recorded and decide nothing.
- **ForceKill.** B only. An owned, TERM-ignoring root is killed with `OwnedChild::kill()`; the time
  runs until `exited()` resolves. 200 samples, p99 < 1 s; a sample with no observed exit counts as
  the largest value.
- **Guard PSS.** B only. 100 owned targets held 180 s; each guard's `smaps_rollup` Pss; median
  ≤ 4 MiB and p95 ≤ 8 MiB.

## What the aggregator rejects

A missing, duplicated, failed or timed-out round; a smoke (`--smoke`) file; a spec, schema or
conditions digest that differs; a sample count below the spec, a gap in the pair index or a zero
duration; a ratio above the limit; a kill p99 at or over 1 s, a lost sample or too few; a PSS
median or p95 above its limit, an unreadable guard, a wrong guard count or a short hold; and
conditions that are incomplete. Quantiles are nearest-rank, every sample the runner recorded is
used (no outlier is removed), and rounds are judged one by one and never pooled
(`tests/perf_gate_aggregate.rs` proves each).

## Running

1. Build both arms; write conditions with `scripts/perf-gate.sh conditions` (once, before any
   sample).
2. `scripts/perf-gate.sh run DIR --runner EXE --a-exe X --b-exe Y` on the idle host. The round is
   measured once and never repeated; a failed or timed-out round is recorded as failed.
3. `scripts/perf-gate.sh aggregate DIR` writes `summary.json` and the CSVs; raw samples stay in
   the round files.

`scripts/perf-gate.sh smoke` runs a shrunken pass and writes `evidence: false` files that the
aggregator rejects.

## Thread rotation (was "Known issue")

A run used to reach `Failed` after a few dozen turns on one thread. Two product limits explain it;
neither is a bug the check has to work around:

- `BudgetExhausted max_file_rereads=20`: the same file read more than 20 times (the `run-length`
  `read-small` mode, a diagnostic, not a measured cell).
- `ContextWindowExhausted` (projected 201,617 > 200,000 at turn 43 of the `read-big` mode). The
  runner's mock used the tool call id `call` for every call, so compaction saw one group and had no
  safe cut. Every tool call id is now unique (`call-<n>`), like a real provider's; with that,
  `read-big` completes 150 turns.

Product limits are unchanged and the active agents read a different file every tick. The runner
still rotates to a fresh thread every 20 turns (probes and agents, both arms, recorded in
conditions.json) so that compaction work is not an uncontrolled load in the measurement. The
terminal reason is not shown by the UI (the protocol exposes only the `Failed` phase): a product
follow-up.

## Result

See the PR description and the measurement's `conditions.json` / `summary.json` (paths there). It
is recorded once and never edited after the fact.
