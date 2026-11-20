# F4-P: product fixture and first measurements

Base: `origin/main` 8ec04796 (includes I0 #711, #713, #714 and the planned-shutdown
watchdog #716). Conditions were fixed and committed before the first run and
extended by two addenda before the reruns: [conditions.md](conditions.md). Raw
samples: `samples-<profile>-<limited|loaded>.csv`; summaries: `summary-*.csv`.
All numbers here come from the latest rerun, on this base and with the final fixture.

**F4-P is not F4.** Nothing here is product -> product evidence. Every
number below is either a launcher-API measurement against the product
artifact, a product-only boot, or the product's side of an exec from a
**harness** old core (this test binary). F4-E starts after 5a.

## Artifacts

Built separately from every test build, without features, serially with
`CARGO_BUILD_GC_ENABLED=false`; nothing is stripped.

| profile | how | ELF size (bytes) | build identity (SHA-256) |
|---|---|---:|---|
| release | sealed by I0's `scripts/product-artifact.sh build` (`cargo build --locked --release -p evorch-core --bin evorch-core`), read through `EVORCH_PRODUCT_CORE_EXE` / `EVORCH_PRODUCT_MANIFEST` | 122,896,728 | `ce0b0eb08cc8441cadaf4074706b09bc5fb0033d8745f20c662cd8d2ae7cca8b` |
| debug (measurement only) | `cargo build -p evorch-core --bin evorch-core`, byte-identical copy + measurement-only manifest (`EVORCH_F4P_MEASURE_MANIFEST`); the sealed contract covers release only | 569,083,112 | `4fe28f0945aedcde8653a65076df39ade528ecc17415d5950e8cd2d29971e726` |

rustc 1.99.0, turtton/cargo 0.102.0-9-turtton.4, `x86_64-unknown-linux-gnu`,
features: none. The release build here ran with the dev shell's
`RUSTFLAGS=-C link-args=-fuse-ld=mold` (recorded in the manifest).

Feature unification is real (observed on an earlier base): `cargo test -p
evorch-core` rebuilt `target/debug/evorch-core` with the test features of that
invocation. That binary was 571,901,416 bytes with a different hash from the
568,929,232-byte product built alone. That is why the fixture accepts only a
separately built artifact.

## Results

Times are milliseconds. "first": 5 fresh copies (n=5, so p95 = max); "warm":
20 samples on a used copy. No sample was discarded and **no sample failed or
was invalid**. `limited` = 4 CPUs, 8 GiB, 4096 tasks; `loaded` = the same plus 3
niced CPU stressors in the same scope (both through
`scripts/reproduce-under-limits.sh`).

### debug / limited

| interval | first n | first p50 / max ms | warm n | warm p50 / p95 / max ms | failures |
|---|---:|---:|---:|---:|---:|
| `preflight` | 5 | 1554.9 / 1615.8 | 20 | 1543.3 / 1691.9 / 1707.1 | 0 |
| `reprobe_before_capture` | 5 | 1582.5 / 1598.0 | 20 | 1551.0 / 1652.7 / 1748.0 | 0 |
| `before_exec_check` | 5 | 516.5 / 532.6 | 20 | 507.0 / 550.6 / 565.1 | 0 |
| `describe_probe_run` | 5 | 266.5 / 268.4 | 20 | 265.3 / 288.5 / 291.6 | 0 |
| `hash_pass` | 5 | 258.3 / 259.9 | 20 | 258.1 / 283.2 / 299.9 | 0 |
| `cold_spawn_to_ready` | 5 | 397.3 / 397.7 | 20 | 392.7 / 417.2 / 419.8 | 0 |
| `exec_to_ready` | 5 | 362.4 / 373.7 | 20 | 361.6 / 391.8 / 397.5 | 0 |
| `exec_to_claim` | 5 | 301.0 / 311.0 | 20 | 298.0 / 329.0 / 330.0 | 0 |

### debug / loaded

| interval | first n | first p50 / max ms | warm n | warm p50 / p95 / max ms | failures |
|---|---:|---:|---:|---:|---:|
| `preflight` | 5 | 1568.7 / 1583.3 | 20 | 1574.7 / 1667.9 / 1676.0 | 0 |
| `reprobe_before_capture` | 5 | 1567.8 / 1575.6 | 20 | 1561.8 / 1586.2 / 1647.9 | 0 |
| `before_exec_check` | 5 | 518.5 / 523.0 | 20 | 518.9 / 532.6 / 533.5 | 0 |
| `describe_probe_run` | 5 | 267.8 / 271.1 | 20 | 266.4 / 277.9 / 367.4 | 0 |
| `hash_pass` | 5 | 258.1 / 262.7 | 20 | 258.3 / 264.1 / 265.2 | 0 |
| `cold_spawn_to_ready` | 5 | 407.3 / 423.8 | 20 | 407.0 / 435.7 / 486.2 | 0 |
| `exec_to_ready` | 5 | 370.0 / 466.3 | 20 | 368.0 / 399.7 / 405.5 | 0 |
| `exec_to_claim` | 5 | 305.0 / 388.0 | 20 | 300.0 / 321.0 / 323.0 | 0 |

### release / limited

| interval | first n | first p50 / max ms | warm n | warm p50 / p95 / max ms | failures |
|---|---:|---:|---:|---:|---:|
| `preflight` | 5 | 365.7 / 403.1 | 20 | 378.7 / 476.8 / 559.8 | 0 |
| `reprobe_before_capture` | 5 | 374.6 / 395.2 | 20 | 378.8 / 485.9 / 494.2 | 0 |
| `before_exec_check` | 5 | 122.4 / 129.0 | 20 | 123.4 / 141.2 / 144.2 | 0 |
| `describe_probe_run` | 5 | 66.5 / 73.9 | 20 | 65.8 / 82.4 / 120.5 | 0 |
| `hash_pass` | 5 | 60.8 / 69.4 | 20 | 61.1 / 69.2 / 95.2 | 0 |
| `cold_spawn_to_ready` | 5 | 163.4 / 186.4 | 20 | 164.4 / 360.9 / 445.0 | 0 |
| `exec_to_ready` | 5 | 108.7 / 200.3 | 20 | 106.1 / 143.7 / 209.4 | 0 |
| `exec_to_claim` | 5 | 85.0 / 102.0 | 20 | 80.0 / 107.0 / 159.0 | 0 |

### release / loaded

| interval | first n | first p50 / max ms | warm n | warm p50 / p95 / max ms | failures |
|---|---:|---:|---:|---:|---:|
| `preflight` | 5 | 381.9 / 390.3 | 20 | 380.0 / 403.9 / 404.2 | 0 |
| `reprobe_before_capture` | 5 | 377.8 / 383.9 | 20 | 378.3 / 399.5 / 403.5 | 0 |
| `before_exec_check` | 5 | 121.3 / 128.3 | 20 | 124.0 / 132.8 / 135.2 | 0 |
| `describe_probe_run` | 5 | 64.7 / 66.5 | 20 | 65.9 / 71.8 / 74.4 | 0 |
| `hash_pass` | 5 | 60.7 / 61.8 | 20 | 61.6 / 66.3 / 67.4 | 0 |
| `cold_spawn_to_ready` | 5 | 172.8 / 219.5 | 20 | 175.2 / 227.2 / 236.2 | 0 |
| `exec_to_ready` | 5 | 104.0 / 110.9 | 20 | 105.4 / 126.4 / 127.9 | 0 |
| `exec_to_claim` | 5 | 78.0 / 83.0 | 20 | 80.0 / 94.0 / 95.0 | 0 |

No sample stands far above its neighbours in this run. The previous run (older
base, superseded) had single samples up to 2.97 s (`exec_to_ready`) in the debug
limited condition; they did not recur and their cause was never found, so
this table is a sample, not a proof that they cannot happen. The `loaded`
condition did **not** slow the intervals measurably, and whether the stressors
really competed for the 4-CPU quota was not verified beyond the script's own
setup: do not read the loaded rows as a contention bound.

### What the exec intervals are

`exec_to_ready` and `exec_to_claim` start at the instant the watcher thread had
**read the pipe EOF**, an observation instant that is later than the kernel's
close during `execve` by the watcher's wake-up latency (not measured). They are
therefore under-measured by that latency, and the readiness line is likewise
seen a reader-thread wake-up after it was written. Neither is claimed to be an
upper or lower bound of the true interval. `exec_to_claim` additionally derives
the claim instant from the successor's lease (`expires_at - lease_ms`), which is
an upper bound of that instant only while the lease did not move
(`claim_stable=true` in every sample). Negative intervals would be recorded as
invalid samples (none occurred).

### Intervals against the deadlines that must not move

Maximum over all four runs of each profile, and the margin to the unchanged limit.

| interval / limit | debug max | release max | margin (debug / release) |
|---|---:|---:|---|
| `describe_probe_run` vs probe deadline **5 s** | 367.4 | 120.5 | 4.63 s / 4.88 s |
| `preflight` (no limit of its own; target timeout 300 s) | 1707.1 | 559.8 | - |
| `reprobe_before_capture` | 1748.0 | 494.2 | - |
| `before_exec_check` | 565.1 | 144.2 | - |
| `exec_to_ready` vs boot deadline **120 s** | 466.3 | 209.4 | 119.53 s / 119.79 s, successor side only (see below) |
| `exec_to_claim` vs lease + grace **15 s** | 388.0 | 159.0 | 14.61 s / 14.84 s; 13.61 s / 13.84 s if the last old heartbeat was one `heartbeat_ms` earlier |
| one hash pass `H` | 299.9 | 95.2 | - |
| `cold_spawn_to_ready` (reference) | 486.2 | 445.0 | - |

Reading the margins:

- The 5 s probe deadline covers one `--describe-handoff` run, which hashes
  the whole ELF once in the product. Measured on a warm page cache (not
  dropped). The run is I/O-bound by the read of the ELF: the debug binary is
  569 MB, so the **extrapolation** (not measured) is that a read below roughly
  114 MB/s would exceed 5 s for the debug binary and below roughly 25 MB/s for
  the release binary. A cold cache on slow storage is therefore the real risk
  to the unchanged probe deadline; this fixture cannot show it because it does
  not drop the host page cache.
- The 120 s deadline starts when `Restarting` is established in the OLD core,
  before exec preparation. This harness measures only the part after the
  exec. Capture stages and `Restarting -> exec` of a production old core are
  **not measured** (5a needed), so the 120 s margin above is not a proven
  product -> product margin.
- Before exec the production `current` target adds hashing of the product's
  own ELF, which the harness cannot do at product size. Derived (not
  measured), from the launcher code: `preflight` +2H, `reprobe_before_capture`
  +1H, `before_exec_check` +1H. With the maximum H this gives for debug:
  preflight ~2.31 s, re-probe ~2.05 s, before_exec ~0.86 s; for release:
  ~0.75 s, ~0.59 s, ~0.24 s. None of the derived values reaches 5 s, but
  the 5 s limit applies to the describe run only, not to these spans.

No number was used to extend a deadline, and no latency is asserted in any test.

## What ran, and what waits for 5a

Executed here (`fixture` unless noted):

- Artifact: the sealed contract's reader (I0) plus this fixture's own checks:
  absent variables (the fixture's `require()` panics in a child process that
  has `CARGO_BIN_EXE_*` but no sealed-artifact variable), a file that is the
  harness build, and, for the measurement-only manifest, size/hash mismatch,
  feature/stripped/profile/argument rules, modification after open.
- Supervision: signals only through a pidfd; the exit is observed without
  reaping, so the leader's pid and group stay reserved until the cleanup; a
  descendant that was orphaned before anything observed it is reclaimed by
  group, family token or marker (also one that left the session with a
  cleared environment); an unowned process is never touched and a reader held
  open by a stranger is bounded; exit before Ready is an event carrying
  stderr; descriptors without close-on-exec do not reach the child; the
  isolated directories (config, state, cache, home, tmp, project, runtime
  sockets) are removed.
- Product: starts through `--config-dir` in the isolated environment; PID and
  image (same device/inode as the sealed artifact); build identity (manifest
  hash = `--describe-handoff` identity); epoch and core identity from the
  handshake; descendants; descriptors; a chat over the public wire and the
  exact provider request seen by the mock; SIGTERM exit; start failure with a
  bad option leaves nothing.
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
replace the artifact. Samples are saved after every round.
