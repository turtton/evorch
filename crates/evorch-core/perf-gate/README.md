# D-3a2 B3-09 performance gate (runner, fixtures, aggregator)

**Status: implemented, not measured.** No evidence has been collected, so this
makes no performance claim. The parent fixes the conditions (arm source SHAs,
unrelated-diff handling, exact values) before the first sample; `spec.json`
holds the proposed frozen values and its SHA-256 goes into `conditions.json`.

Nothing here is on a product path. The runner is a cargo example, the
aggregator lives in `shared-test-support/`, and no product flag switches the
guard off: arm A is a different sealed build, not a toggled one.

| File | Role |
|---|---|
| `perf-gate/spec.json` | frozen counts, workloads, limits, state shape |
| `examples/perf_gate/{main,arm,provider,fixture,cells,conditions}.rs` | runner |
| `shared-test-support/perf_gate/mod.rs` | aggregator and conditions check |
| `tests/perf_gate_aggregate.rs` | negative tests (run in CI, no timing) |
| `scripts/perf-gate.sh` | driver: `conditions`, `run`, `aggregate`, `smoke` |

## What is measured

- **Arms.** A and B are sealed release cores built by
  `scripts/product-artifact.sh build` with the same toolchain, flags and
  features; their SHA-256 and the sealed guard bundle digest are recorded.
- **Core state (normal API only).** 20 idle threads × 50 messages × 8 KiB
  (25 turns of one padded 8 KiB reply each), a repository of 1000 files ×
  16 KiB with a fixed commit id, two active mock-backed agent threads that
  send a `read` turn every 100 ms, one probe thread rotation for the shell
  workload. Threads rotate every 20 turns for both arms (see "Known issue").
- **Shell RTT.** From the instant the provider flushed the `shell` tool call
  to the instant the request carrying the tool result arrived. It covers the
  core's dispatch, the real `sh -c`, output handling and the way back.
  Workloads: `grep -F -- D-7 fixture.txt`, `git diff --quiet -- tracked.txt`,
  `:`. A sample whose result is not `exit_code: 0` (and, for grep, lacks the
  match) is an error, not a timing.
- **Pairs.** 200 paired samples per workload per round after 20 warm-up
  probes per arm, ABBA order derived from the seed. 3 rounds.
- **Delta.** `spec.json` `delta_definition`: `quantile_difference` (quantile
  of the per-pair B−A differences, recommended) or `difference_of_quantiles`
  (pXX(B) − pXX(A)). Both are always computed and printed.
- **ForceKill.** B only. An owned, TERM-ignoring root is killed with
  `OwnedChild::kill()`; the time runs until `exited()` resolves. ≥200 samples,
  p99 < 1 s, a lost sample counts as the maximum.
- **Guard PSS.** B only. 100 owned targets held 180 s; each guard's
  `smaps_rollup` Pss; median ≤ 4 MiB and p95 ≤ 8 MiB.
- **Thresholds kept from the plan.** delta p50 ≤ 2 ms, delta p95 ≤ 5 ms per
  workload and round.

## Rules the aggregator enforces

Nearest-rank quantiles (per-mille, integer), each round judged on its own, no
outlier removal, no pooling of rounds. A missing, failed, timed-out or
non-evidence (`--smoke`) round, a scope mismatch (cpu.max, memory.max, swap,
pids.max, stress-ng not in the scope's `cgroup.procs`), a sample count below
the spec, an ABBA order that differs from the seed, or conditions that differ
between files fails the gate. Every failing cell is reported.

## Running (after the parent approves the conditions)

1. Build both arms outside the limited scope; write `conditions.json` with
   `scripts/perf-gate.sh conditions`.
2. `nix shell nixpkgs#stress-ng --command scripts/perf-gate.sh run DIR ...`
   outside the evorch sandbox (it needs the user's systemd scope). stress-ng
   is required; there is no busy-loop fallback for evidence. A failed round is
   recorded and never repeated.
3. `scripts/perf-gate.sh aggregate DIR` writes the verdict; raw samples stay in
   the per-round files.

`scripts/perf-gate.sh smoke` runs a shrunken, scope-less pass and writes
`evidence: false` files that the aggregator rejects.

## Known issue

A core run reaches `Failed` after roughly 20–43 turns on one thread when each
turn calls a tool (`read`); text-only turns do not. This reproduces on A and B
and is not diagnosed here. The runner rotates threads so every arm sees the
same pattern; it does not hide the cause (`run-length` is the diagnostic
subcommand).
