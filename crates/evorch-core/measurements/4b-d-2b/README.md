# 4b-D-2b deadline evidence

These are offline measurements, not tests or universal latency bounds. Captured
2026-11-16 on Linux 7.3.1-cachyos, AMD Ryzen 9 7900 (24 logical CPUs), debug
builds. Temporary databases and the I/O load reside on the same ext4 NVMe
filesystem (/dev/nvme1n1p2). No CARGO_TARGET_DIR override was used.

## Methods and raw format

From the worktree, using the repository dev-shell environment:

```sh
cargo run -p sandbox --example ledger_latency -- 100 ledger-normal.csv 0
cargo run -p sandbox --example ledger_latency -- 100 ledger-loaded.csv 24
cargo run -p runtime --features recovery-test-support --example measure_learning_suspend -- 100 learning-normal.csv 0
cargo run -p runtime --features recovery-test-support --example measure_learning_suspend -- 100 learning-loaded.csv 24
cargo run -p recorder --example bridge_latency -- 100 bridge-normal.csv 0
cargo run -p recorder --example bridge_latency -- 100 bridge-loaded.csv 24
```

The last argument starts that many busy-loop CPU threads. Ledger loaded samples
have CPU load only. Learning/bridge loaded samples additionally have a concurrent
loop of `dd if=/dev/zero of=<same-filesystem-file> bs=1M count=64 oflag=direct
conv=fdatasync`. Normal samples have neither synthetic load. Measurements ran
serially, without another Cargo build. The loop is stopped after each experiment.

CSV files have **no header**: cell label, duration. Ledger/bridge durations are
integer microseconds (sub-microsecond parts truncated); learning durations are
floating-point milliseconds. Percentiles use nearest rank (ceil(n*p)-1).
Summary tables round milliseconds to three decimals, so raw microsecond values
may differ from original console output by less than 0.001ms.

- Ledger: 100 samples per 33 cells (3300 per condition), empty case, four child
  shapes (plain, TERM ignoring, grandchild, pipe holder), 1/8/64 children,
  explicit stop/drop, plus 1/8/64/512 workers with single/concurrent join.
  Timing starts only after a **non-consuming stopped-state observation**;
  waitpid/waitid do not compete with the owner reaper. Reap/join errors: zero.
- Learning: real runtime, SQLite learning queue and run journal, Core-origin
  lessons, a mock provider that never returns, and normal cancellation/tails.
  Timing starts immediately before pausing producers and includes pause/suspend registration,
  durable fences and all members' normal return. Seal/capture used by the
  example for cleanup happen **after** the measurement. Cohorts 1/4/20 have
  100 samples each normally. Under CPU+I/O load cohorts 1/4 have 100 each;
  cohort 20 has **57 completed samples**. The experiment was deliberately
  interrupted to fit the implementation time budget; incomplete in-flight
  work is not a sample, and the full 100-round command is not claimed to have
  completed. For n=57 the nearest-rank p99 equals the observed maximum.
- Bridge: normal has 100 samples per 8 cells (800 total): 0/100/1000/10000
  accepted events, either flush barrier or checked subscriber thread drain/join
  and final flush.
  The SQLite writer remains alive for Arm, readback and FailedClosed; neither
  measurement is a writer stop/join.
  The CPU+I/O experiment hit its shell 5-minute limit after the four completed
  0/100-event cells (100 each, 400 total). Larger loaded queues are **unmeasured**;
  partial in-memory cells were not written and are not claimed as samples.
  This incomplete experiment is not a successful 800-sample run.
- An initial ledger run was interrupted by the shell's short default command
  limit; its incomplete output is excluded. Only the full rerun is included.

## Learning results (milliseconds)

| Load | Cohort | n | p50 | p95 | p99 | max |
|---|---:|---:|---:|---:|---:|---:|
| normal | 1 | 100 | 11.731 | 16.041 | 16.537 | 18.810 |
| normal | 4 | 100 | 56.349 | 63.193 | 69.320 | 70.344 |
| normal | 20 | 100 | 292.502 | 328.231 | 362.847 | 381.344 |
| CPU+I/O | 1 | 100 | 50.481 | 193.235 | 216.143 | 250.738 |
| CPU+I/O | 4 | 100 | 288.946 | 537.015 | 845.312 | 1032.061 |
| CPU+I/O | 20 | 57 | 1480.702 | 2276.991 | 2713.902 | 2713.902 |

Adopt **10 seconds** as the private, whole-cohort observation deadline. This is
above 2*p99 (5.428s) of the most heavily loaded measured cohort and 3.68 times
its observed maximum, versus 26.2 times the normal maximum. One absolute timer
includes fences and all members; it is not renewed per member or by a merged
restart request. Exceeding it cancels preparation, not the runtime-owned cleanup.
No cleanup ownership is dropped and no deadline is imposed on terminal return.
No user-visible setting/default is added or changed. The separate 300-second
preparation deadline is unchanged by merged requests and is not a post-capture
cancellation deadline.

## D-2c ledger/bridge decision inputs

Worst-cell p99 / maximum (milliseconds), not pooled percentiles:

| Operation | Normal | Loaded |
|---|---:|---:|
| ledger stop/drop to reap | 3.286 / 4.033 | 22.496 / 25.307 (CPU) |
| worker end to join | 7.141 / 7.249 | 7.658 / 7.863 (CPU) |

Private D-2c candidates are **1s reap** and **1s join**, respectively 39.5x and
127x the measured loaded maxima. They are evidence-backed candidates, **not
connected finalization deadlines in this PR**. Bridge results are below.

Normal bridge worst-cell p99/max: flush 98.811/100.608ms; checked shutdown
97.779/98.549ms. CPU+I/O results (milliseconds):

| Operation | n | p50 | p95 | p99 | max |
|---|---:|---:|---:|---:|---:|
| bridge flush x0 events | 100 | 1.624 | 2.402 | 2.540 | 2.979 |
| bridge try_shutdown x0 events | 100 | 1.637 | 2.676 | 3.088 | 3.097 |
| bridge flush x100 events | 100 | 7.248 | 9.010 | 9.688 | 10.227 |
| bridge try_shutdown x100 events | 100 | 8.042 | 9.219 | 9.543 | 9.667 |

A bridge deadline is **not adopted** from the truncated loaded experiment.
D-2c must finish the large-queue loaded measurements before choosing/wiring it.

These maxima do not establish upper bounds for unbounded child/cohort counts,
real MCP/LSP/JS compositions, other hardware, hostile storage stalls, or the
combined finalization sequence. In particular 64 children / 512 workers / 20
lessons are measured cells, not supported concurrency limits. D-2c must confirm
its actual composition and I/O cases before wiring these deadlines.

## Deferred ownership/finalization boundary

M2's twelve finalization stages and atomic capture remain D-2c together: they
must connect all owned cancel/requeue/dispatch tasks to confirmed drains before
DB closure/capture. This PR's asynchronous resume scan remains runtime-owned;
it is not evidence that the finalization registry has drained. Restart commands,
SafeRestart advertising and real exec remain unavailable. No new path can use
these measurement candidates as permission to cross the capture/exec boundary.
