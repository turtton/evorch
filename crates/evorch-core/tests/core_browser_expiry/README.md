# B3-08b real Chromium and real CEF, navigation complete, then the real ten-second expiry (Linux)

Separate harness=false, release-profile, ignored leaf. It is the expiry half of
B3-08, after the SIGKILL half (`tests/core_browser_sigkill/`, unchanged) and the
B3-07 expiry leaf (`tests/core_expiry/`, unchanged). It reuses, read-only through
`#[path]`: the B3-07 composition worker
(`tests/planned_shutdown_watchdog_composition/composition.rs`, one additive
extension point, see below), the B3-06 kill observer
(`shared-test-support/kill_observer`), and from B3-08 `procs.rs`, `tls.rs`,
`fixtures.rs`.

## What is claimed, and what is not

**Class T.** The owner is the B3-07 composition worker, a release **test**
binary built from the product's components (real `LifecycleDriver`, real restart
drain, the real ten-second watchdog). It is not the product ELF. The engines and
their launch/ownership code are the product's: Chromium is started by the real
`AgentBrowser` (B3-04 owned launch) and CEF by the real `RendererOwner` through
`RuntimePages::open_artifact` (the code behind `inspect_artifact`). The cells are
**not P\*** (no sealed product ELF is started and driven) and prove nothing about
a product-ELF expiry, which is 5a-V T3.

| Cell | Engine and entrance (in the worker) | Extra |
|---|---|---|
| `expiry_chromium_t` | real `AgentBrowser::page` → `navigate` → `snapshot`; the controller's HTTPS fixture; Chromium trusts it only through an NSS database in the cell's own temporary HOME | none |
| `expiry_chromium_stopped_t` | same | the guard subtree members other than the target root and the guard are SIGSTOPped before the window |
| `expiry_cef_t` | real `evorch-renderer` + CEF through `RuntimePages::open_artifact` → `agent().snapshot()` | none |
| `expiry_cef_stopped_t` | same | same SIGSTOP |

Chromium and CEF are separate paths; neither stands in for the other. The
children are owned through the typed owner API but are in no ledger the host
cleanup drains (the worker holds a private `SpawnLedger`, or none), so the
stalled cleanup never reaches them: only the guard can reap them after the
worker's `_exit(74)`.

Not claimed: a product-ELF expiry; that cleanup or lease release succeeded
(code 74 certifies neither); any guarantee for `setsid`-escaped processes (the
crashpad handlers are observed and required to die, as in B3-08, which is not an
ownership claim); speed; the other incarnation, capture-only or signal-only
negatives and the deadline-update path (B3-07 and the unit tests named in
`tests/core_expiry/README.md`; this leaf runs only the capture-first order);
`renderer`/`CEF` failure injection (B3-08).

## Sequence and events

1. The worker finishes bootstrap, runs the registered fixture, and prints
   `NAVIGATED <engine> <heading>` **only after reading the page back** from the
   engine (Chromium `snapshot` carries the heading; CEF `snapshot` carries a
   heading that the page's script writes after it touched 384 MiB).
2. The controller requires that event and an observation of its own: for
   Chromium its HTTPS fixture received `GET /b3-08`; for CEF a guard-owned member
   other than the root and the guard holds at least 300 MiB resident (only the
   page's script does that; the B3-08 `cef_p` cell checks that no such member
   exists without a script). Engine processes are also required (renderer and
   GPU/utility for Chromium, zygote-forked and utility for CEF).
3. The worker enters the stage-11 stall (`BLOCKED stopped_join`), which comes
   **after** the irreversible capture, hence after the navigation. The
   controller requires this event second, then pins the owner-rooted tree by
   pidfd (re-validated through `/proc`), the target root, the guard
   (`--launch`, owner argv equals the worker) and every descendant, with
   PID/PPID/PGID/SID/starttime, and pins processes outside that graph whose
   command line carries the cell's private directory.
4. `_stopped_` cells SIGSTOP the members other than root and guard.
5. The controller waits 11 s (more than the watchdog): the worker, root and
   guard must still be alive, and stopped members must still be alive (a stopped
   member cannot exit by itself, so only a kill can end it). That is the
   capture-only negative, which has no event.
6. SIGTERM to the worker. The lower bound is causal: the worker may not die
   earlier than 10.000 s after the moment just before the send. There is **no
   upper bound** on speed; only failure backstops (below).
7. Exit status 74 with no signal and the fixed stderr diagnostic. Then every
   pinned member, the guard and every pinned process outside the graph must be
   **dead by pidfd readiness**, before any cleanup. A process whose parent or
   group changed is not dead. A final scan finds no process of the cell.

## Bounds

Failure watchdogs only: navigation event 100 s (the worker's own bound is
90 s), `BLOCKED` 60 s, exit 74 after the signal 30 s, each death/reap 30 s, the
whole cell 300 s, then 30 s of controller cleanup and 5 s of runtime shutdown.
The nextest override for this binary is 420 s. Cleanup uses retained pidfds and a
subreaper controller, never an unbounded `waitid`; it runs after success,
failure and panic. No virtual time is used with the real I/O.

## Extension point in the composition worker

`composition.rs` gained one `OnceLock` (`OWNED_FIXTURE`) and a four-line call
site after the existing owned-child block. It is unset in every other includer
(`planned_shutdown_watchdog_composition`, `core_expiry`), where it is a no-op.
This leaf registers `fixture::start` before calling `composition::run`.

## Running locally

Seal the product (`scripts/product-artifact.sh build DIR`, export its
variables; the cells need only its guard bundle). Chromium needs `CHROME` and
`certutil` (`EVORCH_B3_CERTUTIL`), CEF needs `EVORCH_RENDERER_EXE` with
`evorch-cef-helper` beside it. A missing one fails the cell; nothing skips.

```sh
timeout -k 30 900 scripts/guard-cargo.sh run -p evorch-core --release \
  --features bootstrap-test-support --locked --test core_browser_expiry \
  -E 'test(=expiry_chromium_t)' --run-ignored only --test-threads 1 --no-tests fail
```

`EVORCH_B3_EVIDENCE_DIR` keeps one JSON per cell (`ready`, `death`).

## Mutations (disposable checkout; code is not committed)

Each is applied after the cell started the engines and must fail the cell for
the intended reason; results are in the PR. Watcher disabled (the watchdog never
fires: no exit 74 within the backstop). Guard removed (`Lease::force` / the
group kill as a no-op: the root or the stopped members survive the worker's
death). Navigation omitted (the fixture starts the engine but does not
navigate: no `NAVIGATED`, or no fixture request / no memory observable). Also
useful: the watchdog deadline shortened below ten seconds (the lower bound
fails).
