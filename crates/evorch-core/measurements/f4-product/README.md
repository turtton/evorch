# F4-P: product fixture and first measurements

Base: `origin/main` 75de89fa (includes I0 #711, #713, #714, the planned-shutdown
watchdog #716 and #717). Conditions were fixed and committed before the first
run and extended by three addenda before the reruns: [conditions.md](conditions.md).
Raw samples: `samples-<profile>-<limited|loaded>.csv`; summaries: `summary-*.csv`.
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
| release | sealed by I0's `scripts/product-artifact.sh build` (`cargo build --locked --release -p evorch-core --bin evorch-core`), read through `EVORCH_PRODUCT_CORE_EXE` / `EVORCH_PRODUCT_MANIFEST` | 122,903,928 | `24a09549fdb916df1dfafb4df6d7d22dd5c53341d093e4852a2b7443e4e6ce27` |
| debug (measurement only) | `cargo build -p evorch-core --bin evorch-core`, byte-identical copy + measurement-only manifest (`EVORCH_F4P_MEASURE_MANIFEST`); the sealed contract covers release only | 569,099,160 | `51ccf465a4fdd811dc246b8cc88bca95ec3a35bbbeda3259c8d92f15fd2e9427` |

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
| `preflight` | 5 | 1780.3 / 2318.1 | 20 | 1751.9 / 1963.2 / 2900.5 | 0 |
| `reprobe_before_capture` | 5 | 1678.8 / 2023.5 | 20 | 1674.7 / 2089.8 / 2272.3 | 0 |
| `before_exec_check` | 5 | 540.3 / 711.8 | 20 | 577.4 / 748.3 / 818.8 | 0 |
| `describe_probe_run` | 5 | 273.3 / 444.0 | 20 | 298.6 / 358.9 / 365.6 | 0 |
| `hash_pass` | 5 | 277.8 / 417.2 | 20 | 294.3 / 334.0 / 356.7 | 0 |
| `cold_spawn_to_ready` | 5 | 793.7 / 864.5 | 20 | 583.0 / 1108.7 / 3253.4 | 0 |
| `exec_to_ready` | 5 | 388.5 / 677.8 | 20 | 401.8 / 745.0 / 851.4 | 0 |
| `exec_to_claim` | 5 | 316.0 / 448.0 | 20 | 328.0 / 489.0 / 596.0 | 0 |

### debug / loaded

| interval | first n | first p50 / max ms | warm n | warm p50 / p95 / max ms | failures |
|---|---:|---:|---:|---:|---:|
| `preflight` | 5 | 1782.3 / 1991.7 | 20 | 1878.9 / 2067.1 / 2095.1 | 0 |
| `reprobe_before_capture` | 5 | 1701.7 / 1873.4 | 20 | 1936.5 / 2122.1 / 2139.6 | 0 |
| `before_exec_check` | 5 | 569.2 / 580.9 | 20 | 612.0 / 685.0 / 710.2 | 0 |
| `describe_probe_run` | 5 | 303.0 / 338.0 | 20 | 313.8 / 356.2 / 371.1 | 0 |
| `hash_pass` | 5 | 284.2 / 339.1 | 20 | 313.4 / 358.2 / 383.6 | 0 |
| `cold_spawn_to_ready` | 5 | 714.8 / 3922.1 | 20 | 667.8 / 1331.7 / 4170.8 | 0 |
| `exec_to_ready` | 5 | 647.4 / 1740.6 | 20 | 550.6 / 1106.6 / 3179.1 | 0 |
| `exec_to_claim` | 5 | 447.0 / 659.0 | 20 | 426.0 / 584.0 / 2351.0 | 0 |

### release / limited

| interval | first n | first p50 / max ms | warm n | warm p50 / p95 / max ms | failures |
|---|---:|---:|---:|---:|---:|
| `preflight` | 5 | 341.0 / 345.2 | 20 | 343.8 / 347.3 / 349.3 | 0 |
| `reprobe_before_capture` | 5 | 342.1 / 345.2 | 20 | 343.0 / 348.7 / 349.3 | 0 |
| `before_exec_check` | 5 | 112.0 / 113.3 | 20 | 112.9 / 115.2 / 115.4 | 0 |
| `describe_probe_run` | 5 | 59.4 / 59.7 | 20 | 59.9 / 60.8 / 60.9 | 0 |
| `hash_pass` | 5 | 55.7 / 56.2 | 20 | 56.3 / 57.2 / 57.4 | 0 |
| `cold_spawn_to_ready` | 5 | 171.3 / 301.1 | 20 | 165.4 / 175.4 / 178.4 | 0 |
| `exec_to_ready` | 5 | 95.7 / 96.5 | 20 | 97.7 / 102.0 / 102.7 | 0 |
| `exec_to_claim` | 5 | 72.0 / 74.0 | 20 | 73.0 / 75.0 / 78.0 | 0 |

### release / loaded

| interval | first n | first p50 / max ms | warm n | warm p50 / p95 / max ms | failures |
|---|---:|---:|---:|---:|---:|
| `preflight` | 5 | 348.3 / 352.0 | 20 | 345.5 / 354.9 / 354.9 | 0 |
| `reprobe_before_capture` | 5 | 346.3 / 423.3 | 20 | 344.4 / 353.0 / 354.4 | 0 |
| `before_exec_check` | 5 | 113.8 / 121.1 | 20 | 112.8 / 116.6 / 117.0 | 0 |
| `describe_probe_run` | 5 | 60.5 / 61.0 | 20 | 59.8 / 61.8 / 65.9 | 0 |
| `hash_pass` | 5 | 57.3 / 57.7 | 20 | 56.1 / 58.6 / 58.6 | 0 |
| `cold_spawn_to_ready` | 5 | 173.4 / 331.2 | 20 | 168.7 / 190.5 / 612.6 | 0 |
| `exec_to_ready` | 5 | 99.0 / 128.8 | 20 | 96.3 / 100.4 / 102.4 | 0 |
| `exec_to_claim` | 5 | 75.0 / 104.0 | 20 | 73.0 / 76.0 / 77.0 | 0 |

**Run-to-run variation is large on this shared host.** The previous run (base
8ec04796, otherwise the same method) had debug maxima of 486 ms for
`cold_spawn_to_ready` and 466 ms for `exec_to_ready`; this run has
4170.8 ms and 3179.1 ms in the debug rows, and the debug runs took 400-450 s
instead of about 310 s. The host is shared with other work; this was neither
controlled nor investigated. The release rows moved much less. The maxima below
are the largest of this run and are a sample, not a bound. The `loaded`
condition is not consistently slower than `limited`, and whether the stressors
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
| `describe_probe_run` vs probe deadline **5 s** | 444.0 | 65.9 | 4.56 s / 4.93 s |
| `preflight` (no limit of its own; target timeout 300 s) | 2900.5 | 354.9 | - |
| `reprobe_before_capture` | 2272.3 | 423.3 | - |
| `before_exec_check` | 818.8 | 121.1 | - |
| `exec_to_ready` vs boot deadline **120 s** | 3179.1 | 128.8 | 116.82 s / 119.87 s, successor side only (see below) |
| `exec_to_claim` vs lease + grace **15 s** | 2351.0 | 104.0 | 12.65 s / 14.90 s; 11.65 s / 13.90 s if the last old heartbeat was one `heartbeat_ms` earlier |
| one hash pass `H` | 417.2 | 58.6 | - |
| `cold_spawn_to_ready` (reference) | 4170.8 | 612.6 | - |

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
  preflight ~3.73 s, re-probe ~2.69 s, before_exec ~1.24 s; for release:
  ~0.47 s, ~0.48 s, ~0.18 s. None of the derived values reaches 5 s, but
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
  reaping, so the leader's pid and group stay reserved until the cleanup. A
  process is reclaimed only if it descends from the harness (directly or by
  subreaper adoption), started with the family, is not in another live
  family's tree, and carries the family's identity: the leader's process group,
  a per-family environment token, or a family marker on a path boundary in its
  command line or working directory. A string in an argument is never enough:
  a process that was already running (a user's `tail -f <root>/log`), one in a
  sibling directory with the same prefix, and another family's tree are left
  alone (tests). A descendant orphaned before anything observed it is
  reclaimed, also one that left the session with a cleared environment. A
  reader held open by a stranger is bounded. A spawn whose pidfd setup fails
  takes the started child back (tests inject both failing steps). Exit before
  Ready is an event carrying stderr; descriptors without close-on-exec do not
  reach the child; the isolated directories are removed. Not covered: a process
  that is not a descendant cannot be created from inside a subreaper, so that
  case is a table test of the decision, and descendants of the harness that
  merely share a tick of the 10 ms process clock with the leader cannot be told
  apart by time.
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
