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
| `preflight` | 5 | 1734.3 / 1768.7 | 20 | 1689.3 / 1774.1 / 1789.8 | 0 |
| `reprobe_before_capture` | 5 | 1747.0 / 1777.6 | 20 | 1675.1 / 1785.3 / 1803.8 | 0 |
| `before_exec_check` | 5 | 580.5 / 589.9 | 20 | 563.0 / 589.0 / 589.9 | 0 |
| `describe_probe_run` | 5 | 296.1 / 303.2 | 20 | 285.1 / 304.8 / 306.4 | 0 |
| `hash_pass` | 5 | 286.1 / 293.2 | 20 | 281.2 / 296.8 / 299.6 | 0 |
| `cold_spawn_to_ready` | 5 | 378.7 / 391.4 | 20 | 376.1 / 403.5 / 420.1 | 0 |
| `exec_to_ready` | 5 | 384.8 / 399.7 | 20 | 379.4 / 398.5 / 398.6 | 0 |
| `exec_to_claim` | 5 | 324.0 / 336.0 | 20 | 318.0 / 336.0 / 337.0 | 0 |

### debug / loaded

| interval | first n | first p50 / max ms | warm n | warm p50 / p95 / max ms | failures |
|---|---:|---:|---:|---:|---:|
| `preflight` | 5 | 1548.9 / 1576.4 | 20 | 1526.6 / 1555.6 / 1571.3 | 0 |
| `reprobe_before_capture` | 5 | 1548.2 / 1571.2 | 20 | 1523.8 / 1570.1 / 1574.6 | 0 |
| `before_exec_check` | 5 | 514.7 / 519.1 | 20 | 506.1 / 519.1 / 521.5 | 0 |
| `describe_probe_run` | 5 | 264.6 / 266.0 | 20 | 259.7 / 269.1 / 270.2 | 0 |
| `hash_pass` | 5 | 257.0 / 259.8 | 20 | 252.2 / 259.6 / 261.0 | 0 |
| `cold_spawn_to_ready` | 5 | 390.5 / 399.2 | 20 | 385.9 / 406.7 / 429.5 | 0 |
| `exec_to_ready` | 5 | 362.7 / 379.0 | 20 | 354.8 / 362.7 / 372.1 | 0 |
| `exec_to_claim` | 5 | 298.0 / 310.0 | 20 | 293.0 / 299.0 / 302.0 | 0 |

### release / limited

| interval | first n | first p50 / max ms | warm n | warm p50 / p95 / max ms | failures |
|---|---:|---:|---:|---:|---:|
| `preflight` | 5 | 390.1 / 394.4 | 20 | 382.6 / 392.9 / 399.8 | 0 |
| `reprobe_before_capture` | 5 | 378.9 / 391.0 | 20 | 378.5 / 395.7 / 398.4 | 0 |
| `before_exec_check` | 5 | 127.5 / 128.9 | 20 | 128.2 / 130.9 / 131.1 | 0 |
| `describe_probe_run` | 5 | 64.5 / 66.0 | 20 | 67.3 / 68.7 / 69.6 | 0 |
| `hash_pass` | 5 | 64.5 / 64.9 | 20 | 64.9 / 65.4 / 65.6 | 0 |
| `cold_spawn_to_ready` | 5 | 153.7 / 172.3 | 20 | 152.6 / 174.9 / 176.7 | 0 |
| `exec_to_ready` | 5 | 102.7 / 104.2 | 20 | 102.0 / 106.8 / 111.8 | 0 |
| `exec_to_claim` | 5 | 80.0 / 81.0 | 20 | 79.0 / 85.0 / 90.0 | 0 |

### release / loaded

| interval | first n | first p50 / max ms | warm n | warm p50 / p95 / max ms | failures |
|---|---:|---:|---:|---:|---:|
| `preflight` | 5 | 335.8 / 337.6 | 20 | 338.1 / 343.0 / 346.7 | 0 |
| `reprobe_before_capture` | 5 | 334.8 / 354.8 | 20 | 337.8 / 342.0 / 344.4 | 0 |
| `before_exec_check` | 5 | 110.3 / 111.3 | 20 | 111.0 / 112.0 / 115.4 | 0 |
| `describe_probe_run` | 5 | 58.2 / 58.8 | 20 | 58.9 / 60.7 / 61.4 | 0 |
| `hash_pass` | 5 | 54.7 / 55.0 | 20 | 55.4 / 57.2 / 57.2 | 0 |
| `cold_spawn_to_ready` | 5 | 153.7 / 172.3 | 20 | 155.4 / 183.4 / 187.8 | 0 |
| `exec_to_ready` | 5 | 94.4 / 95.7 | 20 | 95.1 / 101.2 / 105.8 | 0 |
| `exec_to_claim` | 5 | 71.0 / 72.0 | 20 | 72.0 / 75.0 / 75.0 | 0 |
**Run-to-run variation on this shared host is real.** Earlier runs of the same
method gave debug maxima for `cold_spawn_to_ready` of 486 ms, 4171 ms and
535 ms (superseded runs on earlier fixtures); this run has 429.5 ms. Causes
were not investigated; the host is shared with other work. The maxima below are
the largest of this run and are a sample, not a bound. The `loaded` condition is
not consistently slower than `limited`, and whether the stressors really
competed for the 4-CPU quota was not verified beyond the script's own setup: do
not read the loaded rows as a contention bound.

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
| `describe_probe_run` vs probe deadline **5 s** | 306.4 | 69.6 | 4.69 s / 4.93 s |
| `preflight` (no limit of its own; target timeout 300 s) | 1789.8 | 399.8 | - |
| `reprobe_before_capture` | 1803.8 | 398.4 | - |
| `before_exec_check` | 589.9 | 131.1 | - |
| `exec_to_ready` vs boot deadline **120 s** | 399.7 | 111.8 | 119.60 s / 119.89 s, successor side only (see below) |
| `exec_to_claim` vs lease + grace **15 s** | 337.0 | 90.0 | 14.66 s / 14.91 s; 13.66 s / 13.91 s if the last old heartbeat was one `heartbeat_ms` earlier |
| one hash pass `H` | 299.6 | 65.6 | - |
| `cold_spawn_to_ready` (reference) | 429.5 | 187.8 | - |

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
  preflight ~2.39 s, re-probe ~2.10 s, before_exec ~0.89 s; for release:
  ~0.53 s, ~0.46 s, ~0.20 s. None of the derived values reaches 5 s, but
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
  closes. Inside the reaper every reap of a child, and every conversion of a
  child's pid into a pidfd that is then signalled, happens under one lock, and
  the kernel confirms that the pidfd names a child of the reaper before it is
  signalled; a JOIN waits without reaping and reaps under the lock, so a JOIN and
  a KILL cannot race on a pid. The harness signals only the leader, through a
  pidfd opened while it was unreaped, and talks to the reaper; a family is
  described by a `LaunchSpec` (program, arguments, environment, working
  directory), not a `Command`, so no setting can be silently dropped. The
  cleanup is bounded: a reaper that stops answering (tested with SIGSTOP) or
  dies before reporting the exit (tested with SIGKILL) is killed through its
  pidfd and reported as `reaper_lost`, never a hang or a panic. Tests (all synchronised by events): an orphan made before anything
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
