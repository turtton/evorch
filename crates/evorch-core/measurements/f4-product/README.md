# F4-P: product fixture and first measurements

Base: `origin/main` 219e3bfd (I0, #711; includes #713 and #714). Conditions were
fixed and committed before the first run and extended by an addendum before the
rerun on this base: [conditions.md](conditions.md). Raw samples:
`samples-<profile>-<limited|loaded>.csv`; summaries: `summary-*.csv`.
All numbers here are from the rerun on this base.

**F4-P is not F4.** Nothing here is product -> product evidence. Every
number below is either a launcher-API measurement against the product
artifact, a product-only boot, or the product's side of an exec from a
**harness** old core (this test binary). F4-E starts after 5a.

## Artifacts

Built separately from every test build, without features, serially with
`CARGO_BUILD_GC_ENABLED=false`; nothing is stripped.

| profile | how | ELF size (bytes) | build identity (SHA-256) |
|---|---|---:|---|
| release | sealed by I0's `scripts/product-artifact.sh build` (`cargo build --locked --release -p evorch-core --bin evorch-core`), read through `EVORCH_PRODUCT_CORE_EXE` / `EVORCH_PRODUCT_MANIFEST` | 122,856,616 | `ab9a382fe172ceb1e75a7e90b9221378986b52d80092267bc14d8e15d11d4044` |
| debug (measurement only) | `cargo build -p evorch-core --bin evorch-core`, byte-identical copy + measurement-only manifest (`EVORCH_F4P_MEASURE_MANIFEST`); the sealed contract covers release only | 568,968,192 | `19e6c2e9cea38203d745f4d76bef2d180b44fad5a69a3f8989769f0b27bb02dd` |

rustc 1.99.0, turtton/cargo 0.102.0-9-turtton.4, `x86_64-unknown-linux-gnu`,
features: none. The release build here ran with the dev shell's
`RUSTFLAGS=-C link-args=-fuse-ld=mold` (recorded in the manifest).

Feature unification is real (observed on the earlier base, 1ed6c863):
`cargo test -p evorch-core` rebuilt `target/debug/evorch-core` with the test
features of that invocation. That binary was 571,901,416 bytes with a
different hash from the 568,929,232-byte product (2,972,184 bytes more). That
is why the fixture accepts only a separately built artifact.

## Results

Times are milliseconds. "first": 5 fresh copies (n=5, so p95 = max); "warm":
20 samples on a used copy. No sample was discarded and **no sample failed**.
`limited` = 4 CPUs, 8 GiB, 4096 tasks; `loaded` = the same plus 3 niced CPU
stressors in the same scope (both through `scripts/reproduce-under-limits.sh`).

### debug / limited

| interval | first n | first p50 / max ms | warm n | warm p50 / p95 / max ms | failures |
|---|---:|---:|---:|---:|---:|
| `preflight` | 5 | 1878.8 / 1975.2 | 20 | 1700.3 / 1755.2 / 1946.2 | 0 |
| `reprobe_before_capture` | 5 | 1884.3 / 2041.6 | 20 | 1690.8 / 1774.0 / 1834.7 | 0 |
| `before_exec_check` | 5 | 598.4 / 689.2 | 20 | 561.8 / 585.0 / 634.1 | 0 |
| `describe_probe_run` | 5 | 339.1 / 450.7 | 20 | 288.1 / 295.3 / 313.9 | 0 |
| `hash_pass` | 5 | 338.7 / 430.0 | 20 | 280.8 / 290.9 / 338.8 | 0 |
| `cold_spawn_to_ready` | 5 | 654.7 / 717.9 | 20 | 406.8 / 767.7 / 2440.4 | 0 |
| `exec_to_ready` | 5 | 583.4 / 2966.6 | 20 | 393.4 / 399.9 / 409.2 | 0 |
| `exec_to_claim` | 5 | 438.0 / 2269.0 | 20 | 325.0 / 332.0 / 333.0 | 0 |

### debug / loaded

| interval | first n | first p50 / max ms | warm n | warm p50 / p95 / max ms | failures |
|---|---:|---:|---:|---:|---:|
| `preflight` | 5 | 1597.7 / 1617.4 | 20 | 1700.5 / 1728.6 / 1739.1 | 0 |
| `reprobe_before_capture` | 5 | 1615.7 / 1624.8 | 20 | 1699.1 / 1727.0 / 1758.8 | 0 |
| `before_exec_check` | 5 | 535.6 / 540.1 | 20 | 562.8 / 574.3 / 574.6 | 0 |
| `describe_probe_run` | 5 | 276.3 / 277.4 | 20 | 289.0 / 296.9 / 308.0 | 0 |
| `hash_pass` | 5 | 267.3 / 268.8 | 20 | 281.3 / 290.3 / 298.5 | 0 |
| `cold_spawn_to_ready` | 5 | 424.5 / 464.0 | 20 | 414.6 / 440.3 / 444.3 | 0 |
| `exec_to_ready` | 5 | 380.3 / 391.5 | 20 | 396.6 / 416.3 / 421.6 | 0 |
| `exec_to_claim` | 5 | 311.0 / 321.0 | 20 | 327.0 / 338.0 / 353.0 | 0 |

### release / limited

| interval | first n | first p50 / max ms | warm n | warm p50 / p95 / max ms | failures |
|---|---:|---:|---:|---:|---:|
| `preflight` | 5 | 342.4 / 346.3 | 20 | 346.8 / 360.0 / 369.8 | 0 |
| `reprobe_before_capture` | 5 | 344.2 / 347.0 | 20 | 348.4 / 356.6 / 371.9 | 0 |
| `before_exec_check` | 5 | 113.2 / 113.8 | 20 | 114.0 / 120.0 / 122.0 | 0 |
| `describe_probe_run` | 5 | 59.1 / 61.5 | 20 | 60.0 / 61.9 / 64.6 | 0 |
| `hash_pass` | 5 | 55.9 / 56.8 | 20 | 57.4 / 58.9 / 60.6 | 0 |
| `cold_spawn_to_ready` | 5 | 159.8 / 179.8 | 20 | 162.8 / 251.5 / 431.8 | 0 |
| `exec_to_ready` | 5 | 98.3 / 101.3 | 20 | 98.7 / 107.5 / 109.9 | 0 |
| `exec_to_claim` | 5 | 73.0 / 74.0 | 20 | 74.0 / 80.0 / 80.0 | 0 |

### release / loaded

| interval | first n | first p50 / max ms | warm n | warm p50 / p95 / max ms | failures |
|---|---:|---:|---:|---:|---:|
| `preflight` | 5 | 385.6 / 386.0 | 20 | 354.3 / 358.4 / 359.3 | 0 |
| `reprobe_before_capture` | 5 | 386.3 / 392.4 | 20 | 352.8 / 380.4 / 460.5 | 0 |
| `before_exec_check` | 5 | 125.7 / 129.6 | 20 | 116.2 / 118.1 / 118.1 | 0 |
| `describe_probe_run` | 5 | 70.3 / 74.3 | 20 | 60.8 / 62.0 / 62.3 | 0 |
| `hash_pass` | 5 | 64.0 / 65.4 | 20 | 58.1 / 59.1 / 60.3 | 0 |
| `cold_spawn_to_ready` | 5 | 174.5 / 207.0 | 20 | 165.7 / 241.3 / 292.0 | 0 |
| `exec_to_ready` | 5 | 108.3 / 114.0 | 20 | 99.7 / 103.1 / 103.2 | 0 |
| `exec_to_claim` | 5 | 82.0 / 84.0 | 20 | 75.0 / 77.0 / 77.0 | 0 |

Outliers are kept. The debug/limited run has three single samples far above
its neighbours (`exec_to_ready` 2966.6 ms,
`exec_to_claim` 2269 ms, `cold_spawn_to_ready` 2440.4 ms against a median
near 0.4 s); the cause was not investigated (a busy host moment is likely, not
shown). The `loaded` condition did **not** slow the intervals measurably, and
whether the stressors really competed for the 4-CPU quota was not verified
beyond the script's own setup: do not read the loaded rows as a contention
bound.

### Intervals against the deadlines that must not move

Maximum over all four runs of each profile, and the margin to the unchanged limit.

| interval / limit | debug max | release max | margin (debug / release) |
|---|---:|---:|---|
| `describe_probe_run` vs probe deadline **5 s** | 450.7 | 74.3 | 4.55 s / 4.93 s |
| `preflight` (no limit of its own; target timeout 300 s) | 1975.2 | 386.0 | - |
| `reprobe_before_capture` | 2041.6 | 460.5 | - |
| `before_exec_check` | 689.2 | 129.6 | - |
| `exec_to_ready` vs boot deadline **120 s** | 2966.6 | 114.0 | 117.03 s / 119.89 s, successor side only (see below) |
| `exec_to_claim` vs lease + grace **15 s** | 2269.0 | 84.0 | 12.73 s / 14.92 s; 11.73 s / 13.92 s if the last old heartbeat was one `heartbeat_ms` earlier |
| one hash pass `H` | 430.0 | 65.4 | - |
| `cold_spawn_to_ready` (reference) | 2440.4 | 431.8 | - |

Reading the margins:

- The 5 s probe deadline covers one `--describe-handoff` run, which hashes
  the whole ELF once in the product. Measured on a warm page cache (not
  dropped). The run is I/O-bound by the read of the ELF: the debug binary is
  569 MB, so the **extrapolation** (not measured) is that a read below roughly
  114 MB/s would exceed 5 s for the debug binary and below roughly 25 MB/s for
  the release binary. A cold cache on slow storage is therefore the real risk
  to the unchanged probe deadline; this fixture cannot show it because it does
  not drop the host page cache.
- `exec_to_claim` is an upper bound of the claim instant (derived from the
  claimed lease; `claim_stable=true` in every sample, so no heartbeat moved it).
  Its resolution is 1 ms.
- The 120 s deadline starts when `Restarting` is established in the OLD core,
  before exec preparation. This harness measures only the part after the
  exec. Capture stages and `Restarting -> exec` of a production old core are
  **not measured** (5a needed), so the 120 s margin above is not a proven
  product -> product margin.
- Before exec the production `current` target adds hashing of the product's
  own ELF, which the harness cannot do at product size. Derived (not
  measured), from the launcher code: `preflight` +2H, `reprobe_before_capture`
  +1H, `before_exec_check` +1H. With the maximum H this gives for debug:
  preflight ~2.84 s, re-probe ~2.47 s, before_exec ~1.12 s; for release:
  ~0.52 s, ~0.53 s, ~0.20 s. None of the derived values reaches 5 s, but
  the 5 s limit applies to the describe run only, not to these spans.

No number was used to extend a deadline, and no latency is asserted in any test.

## What ran, and what waits for 5a

Executed here (`fixture` unless noted):

- Artifact: the sealed contract's reader (I0) plus this fixture's own checks:
  absent variables (the fixture's `require()` panics in a child process that
  has `CARGO_BIN_EXE_*` but no sealed-artifact variable), a file that is the
  harness build, and, for the measurement-only manifest, size/hash mismatch,
  feature/stripped/profile/argument rules, modification after open.
- Supervision: descendants found and killed when the owner is dropped; clean
  exit with no orphans; an orphan left by the child is reported then reaped
  (the test process is a child subreaper, so the join is a kernel wait, not a
  `/proc` poll); exit before Ready is an event carrying stderr; the isolated
  directories (config, state, cache, home, tmp, project, runtime sockets) are
  removed.
- Product: starts through `--config-dir` in the isolated environment; PID and
  image (same device/inode as the sealed artifact); build identity (manifest
  hash = `--describe-handoff` identity); epoch and core identity from the
  handshake; descendants; descriptors (no inherited descriptor above 2 without
  close-on-exec); a chat over the public wire and the exact provider request
  seen by the mock; SIGTERM exit; start failure with a bad option leaves nothing.
- Renderer helper (fake helper of `renderer-test-helper`, a harness binary; the
  test exists only with that feature, as the other renderer process tests):
  the product starts it on `inspect_artifact open`, the run's own event says
  when, a second wire client sees the renderer `Ready`, the product's exit
  and the helper tree's exit are joined.
- **Harness -> product** exec: real `execve` observed by the close-on-exec
  pipe EOF; same PID, image replaced by the artifact; Ready line; owner claim.
- Measurements above.

Waiting for 5a / F4-E (not done, not claimed):

- Product -> product restart through the public request, including the real
  coordinator's preflight/capture/`Restarting`/exec timings of a product old
  core and the 120 s budget as one product-owned span.
- `current`-target hashing at product size (only derived above).
- Two consecutive restarts of the same build, the second-request refusal while
  a graph is undisposed, and owner/FD/socket/helper residue after a product ->
  product exec.
- The real CEF helper: the scenario exists
  (`product_starts_and_takes_back_the_real_renderer_helper`, ignored) but was
  **not verified**: with the real `evorch-renderer` in this sandbox the product
  answered `renderer refused the request: Unavailable`, so no helper ran.

## Reproduce

Release: `export $(scripts/product-artifact.sh build <dir>)`. Debug (measurement
only): build the test binary first, then the product, then run the ignored
`write_measurement_manifest_for_built_artifact` with `EVORCH_F4P_BUILD_ELF`,
`EVORCH_F4P_BUILD_PROFILE`, `EVORCH_F4P_BUILD_ARGS`, `EVORCH_F4P_ARTIFACT_DIR`
(`cargo test` rewrites `target/debug/evorch-core`, so the order matters), and
export `EVORCH_F4P_MEASURE_MANIFEST`. Then:

```sh
EVORCH_F4P_MEASURE_OUT=crates/evorch-core/measurements/f4-product \
EVORCH_F4P_MEASURE_LABEL=limited \
scripts/reproduce-under-limits.sh --cpus 4 --memory 8G --tasks 4096 --timeout 3000 -- \
  <f4_product_fixture test binary> --ignored --exact measure_f4_product_intervals --nocapture
```

(`loaded`: add `--stress-cpus 3` and label `loaded`.) The measurement ran
the test binary directly (not under `cargo`), so that a Cargo rebuild cannot
replace the artifact.

## Note on a later fixture change

CI (nextest on GitHub runners) showed two descriptors without close-on-exec
leaking from the runner into the product. After the measurements above were
taken, `ManagedProcess::spawn` was changed to close, in the child before exec,
the descriptors above 2 that the test process holds without close-on-exec
(tested by `descriptors_without_close_on_exec_do_not_reach_a_child`, which
fails without the change). The measured code paths are otherwise unchanged; a
handful of `close` calls per spawn is not expected to move any interval, but the
tables were not re-taken after it.
