# F4-P: product fixture and first measurements

Base: `origin/main` 75de89fa (includes I0 #711, #713, #714, the planned-shutdown
watchdog #716 and #717). Conditions were fixed and committed before the first
run and extended by four addenda before the reruns: [conditions.md](conditions.md).
Raw samples: `samples-<profile>-<limited|loaded>.csv`; summaries: `summary-*.csv`.
All numbers here come from the latest rerun, on this base and with the final fixture.

**F4-P is not F4.** Nothing here is product -> product evidence. Every
number below is either a launcher-API measurement against the product
artifact, a product-only boot, or the product's side of an exec from a
**harness** old core (this test binary). F4-E starts after 5a.

## Artifacts

Built separately from every test build, without features, serially with
`CARGO_BUILD_GC_ENABLED=false`; nothing is stripped. The product code did not
change between the artifact builds and the final fixture commit (only test
files did), so the artifacts of base 75de89fa are the ones measured.

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
| `preflight` | 5 | 1599.5 / 1671.0 | 20 | 1505.9 / 1620.4 / 1707.8 | 0 |
| `reprobe_before_capture` | 5 | 1575.2 / 1689.1 | 20 | 1503.6 / 1609.9 / 1628.9 | 0 |
| `before_exec_check` | 5 | 525.6 / 554.8 | 20 | 499.7 / 532.8 / 559.4 | 0 |
| `describe_probe_run` | 5 | 264.3 / 286.4 | 20 | 256.2 / 284.7 / 285.1 | 0 |
| `hash_pass` | 5 | 259.5 / 295.8 | 20 | 250.9 / 276.6 / 289.5 | 0 |
| `cold_spawn_to_ready` | 5 | 399.0 / 400.8 | 20 | 375.8 / 387.0 / 387.2 | 0 |
| `exec_to_ready` | 5 | 364.4 / 410.7 | 20 | 347.7 / 389.1 / 393.1 | 0 |
| `exec_to_claim` | 5 | 302.0 / 316.0 | 20 | 288.0 / 329.0 / 334.0 | 0 |

### debug / loaded

| interval | first n | first p50 / max ms | warm n | warm p50 / p95 / max ms | failures |
|---|---:|---:|---:|---:|---:|
| `preflight` | 5 | 1519.9 / 1553.1 | 20 | 1601.4 / 1750.9 / 1769.8 | 0 |
| `reprobe_before_capture` | 5 | 1518.6 / 1552.6 | 20 | 1583.4 / 1688.4 / 1813.4 | 0 |
| `before_exec_check` | 5 | 508.4 / 514.2 | 20 | 523.9 / 553.9 / 572.0 | 0 |
| `describe_probe_run` | 5 | 263.4 / 267.2 | 20 | 270.7 / 291.7 / 296.8 | 0 |
| `hash_pass` | 5 | 255.6 / 257.8 | 20 | 262.3 / 286.6 / 320.3 | 0 |
| `cold_spawn_to_ready` | 5 | 388.6 / 403.4 | 20 | 398.0 / 466.0 / 535.3 | 0 |
| `exec_to_ready` | 5 | 356.0 / 389.3 | 20 | 370.5 / 415.2 / 416.5 | 0 |
| `exec_to_claim` | 5 | 294.0 / 320.0 | 20 | 307.0 / 341.0 / 341.0 | 0 |

### release / limited

| interval | first n | first p50 / max ms | warm n | warm p50 / p95 / max ms | failures |
|---|---:|---:|---:|---:|---:|
| `preflight` | 5 | 336.8 / 400.6 | 20 | 341.5 / 362.7 / 383.3 | 0 |
| `reprobe_before_capture` | 5 | 361.0 / 393.2 | 20 | 335.6 / 383.5 / 383.8 | 0 |
| `before_exec_check` | 5 | 119.3 / 129.9 | 20 | 110.1 / 125.9 / 128.4 | 0 |
| `describe_probe_run` | 5 | 64.6 / 67.8 | 20 | 58.6 / 66.5 / 69.0 | 0 |
| `hash_pass` | 5 | 56.7 / 65.0 | 20 | 55.0 / 63.4 / 63.8 | 0 |
| `cold_spawn_to_ready` | 5 | 164.0 / 175.6 | 20 | 162.1 / 394.2 / 899.2 | 0 |
| `exec_to_ready` | 5 | 96.2 / 106.4 | 20 | 95.6 / 123.0 / 153.6 | 0 |
| `exec_to_claim` | 5 | 72.0 / 80.0 | 20 | 72.0 / 78.0 / 120.0 | 0 |

### release / loaded

| interval | first n | first p50 / max ms | warm n | warm p50 / p95 / max ms | failures |
|---|---:|---:|---:|---:|---:|
| `preflight` | 5 | 371.1 / 375.9 | 20 | 344.0 / 356.5 / 379.9 | 0 |
| `reprobe_before_capture` | 5 | 370.9 / 380.3 | 20 | 341.7 / 352.9 / 366.9 | 0 |
| `before_exec_check` | 5 | 121.7 / 123.7 | 20 | 112.5 / 113.9 / 116.8 | 0 |
| `describe_probe_run` | 5 | 64.3 / 68.2 | 20 | 59.6 / 60.9 / 62.1 | 0 |
| `hash_pass` | 5 | 60.4 / 62.0 | 20 | 56.2 / 58.0 / 58.4 | 0 |
| `cold_spawn_to_ready` | 5 | 161.9 / 175.4 | 20 | 170.4 / 428.2 / 594.6 | 0 |
| `exec_to_ready` | 5 | 103.0 / 108.0 | 20 | 98.7 / 119.3 / 177.9 | 0 |
| `exec_to_claim` | 5 | 79.0 / 82.0 | 20 | 73.0 / 97.0 / 114.0 | 0 |

**Run-to-run variation on this shared host is real.** Two earlier runs of the
same method gave debug maxima for `cold_spawn_to_ready` of 486 ms and 4171 ms
(superseded runs on slightly different fixtures); this run has
535.3 ms. Causes were not investigated; the host is shared with other work. The
maxima below are the largest of this run and are a sample, not a bound. The
`loaded` condition is not consistently slower than `limited`, and whether
the stressors really competed for the 4-CPU quota was not verified beyond the
script's own setup: do not read the loaded rows as a contention bound.

### What the intervals are

- `cold_spawn_to_ready` starts when the family's reaper reports that the leader
  was started (the reaper's own start-up is excluded) and ends when the reader
  thread receives the readiness line.
- `exec_to_ready` and `exec_to_claim` start at the instant the watcher thread had
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
| `describe_probe_run` vs probe deadline **5 s** | 296.8 | 69.0 | 4.70 s / 4.93 s |
| `preflight` (no limit of its own; target timeout 300 s) | 1769.8 | 400.6 | - |
| `reprobe_before_capture` | 1813.4 | 393.2 | - |
| `before_exec_check` | 572.0 | 129.9 | - |
| `exec_to_ready` vs boot deadline **120 s** | 416.5 | 177.9 | 119.58 s / 119.82 s, successor side only (see below) |
| `exec_to_claim` vs lease + grace **15 s** | 341.0 | 120.0 | 14.66 s / 14.88 s; 13.66 s / 13.88 s if the last old heartbeat was one `heartbeat_ms` earlier |
| one hash pass `H` | 320.3 | 65.0 | - |
| `cold_spawn_to_ready` (reference) | 535.3 | 899.2 | - |

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
  preflight ~2.41 s, re-probe ~2.13 s, before_exec ~0.89 s; for release:
  ~0.53 s, ~0.46 s, ~0.19 s. None of the derived values reaches 5 s, but
  the 5 s limit applies to the describe run only, not to these spans.

No number was used to extend a deadline, and no latency is asserted in any test.

## What ran, and what waits for 5a

Executed here (`fixture` unless noted):

- Artifact: the sealed contract's reader (I0) plus this fixture's own checks:
  absent variables (the fixture's `require()` panics in a child process that
  has `CARGO_BIN_EXE_*` but no sealed-artifact variable), a file that is the
  harness build, and, for the measurement-only manifest, size/hash mismatch,
  feature/stripped/profile/argument rules, modification after open.
- Supervision (`process.rs`, `reaper.rs`): each family runs under a reaper
  process of its own, the test binary run again through its
  `f4p_family_reaper` role. The reaper is the subreaper of that one family: it
  starts the leader as its only child, so every other child it ever has is an
  orphan of that leader's tree. Ownership is the reaper's tree and nothing
  else; the harness is not a subreaper and never signals a process it found by
  looking (no marker, token, path or time is used). The reaper kills and reaps
  only its own direct children, holds the leader as a zombie (pid and group
  reserved) until the cleanup, and cleans up by itself when its control pipe
  closes. The harness signals only the leader, through a pidfd, and talks to the
  reaper. Tests (all synchronised by events): an orphan made before anything
  observed it, also one that left the session with a cleared environment,
  is adopted by the reaper (`ppid == reaper`, not the harness) and reclaimed;
  processes that are not in the family are never touched whatever they
  reference (started before or after the family, naming the log path, working
  below the family's directory, in a same-prefix sibling directory); another
  family's orphan survives while its leader is gone and is reclaimed only by its
  own cleanup; a family member that holds the output pipe is killed so the
  readers end, while a stranger that holds it (reopened through `/proc`) is
  bounded and left alone; a failed pidfd setup takes the started family back
  (both failing steps injected); descriptors without close-on-exec do not reach
  the supervised child, while this process's own descriptor flags stay unchanged
  and an unrelated `Command` still inherits them. A reaper that is killed is
  reported (`reaper_lost`), never repaired by guessing.
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
  and the helper tree's exit are joined through the reaper.
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
