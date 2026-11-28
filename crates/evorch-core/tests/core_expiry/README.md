# B3-07 real ten-second expiry and exit 74 (Linux, release only)

Separate `harness = false` leaf. Every cell is an **ignored** test (the debug
inventory never runs it); CI runs it once per event, in a **release** build, in the
`release-expiry` job of `.github/workflows/ci.yml` (`scripts/release-expiry.sh`, which
runs `scripts/guard-cargo.sh run --run-ignored only` over metadata prepared with
`-p evorch-core --release --features bootstrap-test-support --test core_expiry
--test core_browser_expiry`). The controller
refuses to run when built with debug assertions.

## What it is, and is not

Evidence class: **T**. The artifact is the release *test* binary. It links the
same product parts the core links: `run_with_runtime` and the real ten-second
watchdog thread, the real `LifecycleDriver`, the real planned-restart finalization
and storage drain, `OwnedSession::spawn_owned` and the process guard. It is **not
the product ELF** and not P/P\*. The same check on the sealed product ELF is the
5a-V public train's T3 (real restart) and stays an open publication condition. No
product hook, no shortened clock, no feature-gated build is used.

The worker is `composition.rs` of `planned_shutdown_watchdog_composition`, included
by path (only an owned-child fixture and a stdin gate were added there). The
controller reuses the B3-06 `kill_observer` unchanged: pidfd pins, `/proc`
re-validation, reachability from validated rows, a subreaper for cleanup.

## Cells

| Cell | Stall | Sequence | Observed |
|---|---|---|---|
| `expiry_capture_first` | stopped-join (stage 11) | irreversible capture; 11 s with no signal; SIGTERM | worker, root and grandchild alive for the 11 s; then exit 74 at >= 10 s after the send |
| `expiry_signal_first` | flush | SIGTERM pending **before** capture; 11 s at a gate; gate released (capture) | alive during the 11 s; exit 74 at >= 10 s after the release |

In every cell the worker owns a root plus a same-PGID grandchild through the typed
owner API (`OwnedSession::spawn_owned`, ChildKind::Other, no ledger, so cleanup
never reaches them). After exit 74 the root, the grandchild **and the guard** must
be dead by their pidfds. Only then does the controller clean up. The exit status
must be 74 with no signal, and stderr must carry the fixed diagnostic.

Why stopped-join for the capture-first cell: at flush the product's own storage drain has a 10 s
deadline, so a capture-only window of 11 s would end in the product's error
(exit 1), not in the watchdog. Stage 11 has no drain deadline. The pending-signal
cell cannot use stopped-join because the already-pending signal lets the
lifecycle finish first; flush is safe there because the watchdog arms at capture,
before the drain starts, and an expired watchdog wins on every exit path.

## Time

Success comes from events (pidfd readiness, stdout events, the READY connection).
The clock is used in exactly two places, both deliberate:

* A **lower bound**: the process must not die earlier than 10.000 s after the
  moment just before the signal (or gate release) that completed the arming
  condition. That is causal (the watchdog arms after it), so it needs no
  tolerance.
* A **negative has no event**. "Not fired" is shown by staying alive for an 11 s
  window (> 10 s). This is the documented exception to the no-sleep rule.

There is no upper bound on speed. Failure backstops only: READY 60 s, 74 not
observed 30 s after the signal, cell 180 s, cleanup 30 s. Cleanup uses retained
pidfds and a subreaper, never an unbounded `waitid`.

## Not covered, and why

* **A repeated signal must not renew the deadline** (mutation W4). Not reachable
  here: once the watchdog is armed, nothing re-reads the inbox in a stalled cleanup
  (the lifecycle reader is stopped before the stage-11 stall, and the flush stall
  ends the lifecycle's reading). A first attempt (a second SIGTERM, then a delayed
  lifecycle read) raced the stall's own event and was removed. A `Deadline::arm`
  that renews therefore PASSES this leaf. **Parent-approved substitute: this leaf
  does not reach the deadline-update path, so W4 is covered by the unit tests**
  `restart::shutdown_watchdog_tests::deadline_is_ten_seconds_once_and_overflow_fails_closed`
  (state.rs:13, a re-arm at 99 s must keep 10_000_000_017) and
  `...::capture_and_signal_are_order_independent_and_duplicate_does_not_extend`
  (:83, a duplicate signal/capture must keep 10_000_000_700). With W4 applied both
  FAIL (the baseline runs 18/18 PASS); recorded in the PR.
* **Other incarnation** (an exec'd successor does not inherit the capture): left
  to `planned_shutdown_watchdog_composition::product_exec` (product ELF).
  Not duplicated here.
* **Per-root reaping at expiry.** The 11 roots (8 ChildKinds, StdioSession, PTY,
  js-worker) are each proven reaped after core SIGKILL by `core_sigkill` (B3-06).
  This leaf exercises one representative guard-owned root (ChildKind::Other via
  `OwnedSession::spawn_owned`). It relies on the guard's owner-death path being the
  same for `_exit(74)` and SIGKILL; it does not re-run the 11 roots at expiry.
  Real Chromium/CEF after navigation at 74 is B3-08.
* A stall *before* the product's own `ChildrenReaped` stage is not reproduced:
  the children here are outside the host ledger by design, so they model owned
  children that a stalled cleanup never reached.
* The actual `evorch eval run` two-stage owner chain: a separate follow-up on the
  `core_sigkill` leaf.
* Code 74 certifies neither cleanup nor lease release (product README). This leaf
  asserts exit 74, the diagnostic and reaping of guard-owned children, nothing more.

## Runner timeout

`.config/nextest.toml` gives only this binary a 240 s slow-timeout (default 180 s):
a cell's failure backstops end at 180 s and its controller cleanup takes up to
30 s more, which the runner must not cut short.

## Running locally

Build the sealed product (`scripts/product-artifact.sh build DIR`, export its
variables) and run `scripts/guard-cargo.sh run -p evorch-core --release --features
bootstrap-test-support --test core_expiry --run-ignored only --test-threads 1
--no-tests fail` under `timeout -k`. `EVORCH_B3_EVIDENCE_DIR` keeps one JSON per
cell (exit, elapsed, guard identity, dead pids). Local acceptance: three
consecutive passes.

## Mutations (disposable checkout; code is not committed)

Applied to `lifecycle/shutdown_watchdog{,/state}.rs` and `process-guard`. Results
are in the PR. A mutation that does not reach the path is not evidence:
signal-only arming needs the watcher to observe the inbox before capture, so that
mutation also removes the capture gate in `watch`.
