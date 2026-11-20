# F4-P: product fixture and first measurements

Base: `origin/main` 1ed6c863 (#710). Conditions were fixed and committed
before the first run: [conditions.md](conditions.md). Raw samples:
`samples-<profile>-<limited|loaded>.csv`; summaries: `summary-*.csv`.

**F4-P is not F4.** Nothing here is product -> product evidence. Every
number below is either a launcher-API measurement against the product
artifact, a product-only boot, or the product's side of an exec from a
**harness** old core (this test binary). F4-E starts after 5a.

## Artifacts

Built separately, without features, with the shipped arguments, serially with
`CARGO_BUILD_GC_ENABLED=false`; copied byte-for-byte with a manifest. Nothing is stripped.

| profile | arguments | ELF size (bytes) | build identity (SHA-256) |
|---|---|---:|---|
| debug | `cargo build -p evorch-core --bin evorch-core` | 568,929,232 | `8844ecb5ad19df200c6e829c1eb80182dd30bf5f63dd41e344a1ec2d2bec6337` |
| release | `cargo build --release -p evorch-core --bin evorch-core` | 122,847,064 | `cc9548fb113449f6f128031f33688095622a8c8792f615372d4ca602ebaf1cd4` |

Source revision 1ed6c863fe69e058aeb446db1e48c40dc03695af, rustc 1.99.0,
turtton/cargo 0.102.0-9-turtton.4, `x86_64-unknown-linux-gnu`, features: none.

Feature unification is real: `cargo test -p evorch-core` rebuilds
`target/debug/evorch-core` with the test features of that invocation. The
debug binary Cargo handed to the test build was **571,901,416 bytes**, a
different hash from the 568,929,232-byte product (2,972,184 bytes more). That
is why the fixture accepts only a separately built artifact and a manifest.

## Results

Times are milliseconds. "first": 5 fresh copies (n=5, so p95 = max); "warm":
20 samples on a used copy. No sample was discarded and **no sample failed**.
`limited` = 4 CPUs, 8 GiB, 4096 tasks; `loaded` = the same plus 3 niced CPU
stressors in the same scope (both through `scripts/reproduce-under-limits.sh`).

### debug / limited

| interval | first n | first p50 / max ms | warm n | warm p50 / p95 / max ms | failures |
|---|---:|---:|---:|---:|---:|
| `preflight` | 5 | 1761.9 / 1878.1 | 20 | 1618.3 / 1706.3 / 1740.2 | 0 |
| `reprobe_before_capture` | 5 | 1784.5 / 1877.0 | 20 | 1611.7 / 1718.3 / 1752.4 | 0 |
| `before_exec_check` | 5 | 579.5 / 619.1 | 20 | 532.0 / 592.2 / 597.8 | 0 |
| `describe_probe_run` | 5 | 303.2 / 316.0 | 20 | 273.7 / 290.3 / 299.0 | 0 |
| `hash_pass` | 5 | 283.2 / 317.8 | 20 | 270.1 / 286.2 / 299.5 | 0 |
| `cold_spawn_to_ready` | 5 | 390.2 / 409.6 | 20 | 386.7 / 410.8 / 436.5 | 0 |
| `exec_to_ready` | 5 | 411.0 / 419.8 | 20 | 372.6 / 391.7 / 412.6 | 0 |
| `exec_to_claim` | 5 | 348.0 / 359.0 | 20 | 311.0 / 330.0 / 347.0 | 0 |

### debug / loaded

| interval | first n | first p50 / max ms | warm n | warm p50 / p95 / max ms | failures |
|---|---:|---:|---:|---:|---:|
| `preflight` | 5 | 1605.1 / 1610.3 | 20 | 1584.4 / 1612.7 / 1615.9 | 0 |
| `reprobe_before_capture` | 5 | 1605.5 / 1613.9 | 20 | 1590.5 / 1614.5 / 1620.3 | 0 |
| `before_exec_check` | 5 | 533.1 / 535.6 | 20 | 526.4 / 534.1 / 534.5 | 0 |
| `describe_probe_run` | 5 | 273.9 / 274.8 | 20 | 269.7 / 275.9 / 276.6 | 0 |
| `hash_pass` | 5 | 266.8 / 269.0 | 20 | 263.2 / 269.0 / 269.5 | 0 |
| `cold_spawn_to_ready` | 5 | 393.8 / 398.5 | 20 | 391.1 / 400.9 / 408.6 | 0 |
| `exec_to_ready` | 5 | 369.5 / 468.4 | 20 | 363.0 / 370.4 / 379.5 | 0 |
| `exec_to_claim` | 5 | 307.0 / 407.0 | 20 | 302.0 / 307.0 / 308.0 | 0 |

### release / limited

| interval | first n | first p50 / max ms | warm n | warm p50 / p95 / max ms | failures |
|---|---:|---:|---:|---:|---:|
| `preflight` | 5 | 349.6 / 360.9 | 20 | 379.0 / 403.3 / 408.4 | 0 |
| `reprobe_before_capture` | 5 | 353.6 / 368.3 | 20 | 374.2 / 404.9 / 406.7 | 0 |
| `before_exec_check` | 5 | 114.3 / 121.8 | 20 | 123.2 / 135.6 / 136.2 | 0 |
| `describe_probe_run` | 5 | 63.5 / 68.6 | 20 | 65.5 / 71.7 / 73.8 | 0 |
| `hash_pass` | 5 | 57.5 / 66.0 | 20 | 61.7 / 67.4 / 68.0 | 0 |
| `cold_spawn_to_ready` | 5 | 155.9 / 157.6 | 20 | 155.8 / 175.6 / 183.1 | 0 |
| `exec_to_ready` | 5 | 97.2 / 107.1 | 20 | 104.6 / 108.6 / 109.3 | 0 |
| `exec_to_claim` | 5 | 74.0 / 83.0 | 20 | 82.0 / 85.0 / 86.0 | 0 |

### release / loaded

| interval | first n | first p50 / max ms | warm n | warm p50 / p95 / max ms | failures |
|---|---:|---:|---:|---:|---:|
| `preflight` | 5 | 342.6 / 343.4 | 20 | 356.1 / 359.6 / 368.0 | 0 |
| `reprobe_before_capture` | 5 | 343.3 / 345.4 | 20 | 354.6 / 363.3 / 401.2 | 0 |
| `before_exec_check` | 5 | 112.5 / 113.6 | 20 | 116.3 / 120.8 / 141.3 | 0 |
| `describe_probe_run` | 5 | 59.3 / 59.4 | 20 | 62.0 / 77.4 / 85.2 | 0 |
| `hash_pass` | 5 | 56.3 / 56.8 | 20 | 58.3 / 59.3 / 60.2 | 0 |
| `cold_spawn_to_ready` | 5 | 154.7 / 158.2 | 20 | 157.3 / 169.6 / 171.2 | 0 |
| `exec_to_ready` | 5 | 95.4 / 95.8 | 20 | 99.1 / 103.9 / 109.2 | 0 |
| `exec_to_claim` | 5 | 73.0 / 74.0 | 20 | 75.0 / 77.0 / 83.0 | 0 |

The loaded condition did **not** slow the intervals measurably (debug p50 is
within noise of limited; the worst loaded sample is one `exec_to_ready` of
468 ms). Whether the stressors really competed for the 4-CPU quota was not
verified beyond the script's own setup; do not read the loaded rows as a
contention bound.

### Intervals against the deadlines that must not move

Maximum over all four runs of each profile, and the margin to the unchanged limit.

| interval / limit | debug max | release max | margin (debug / release) |
|---|---:|---:|---|
| `describe_probe_run` vs probe deadline **5 s** | 316.0 | 85.2 | 4.68 s (15.8x) / 4.91 s (58.7x) |
| `preflight` (no limit of its own; target timeout 300 s) | 1878.1 | 408.4 | - |
| `reprobe_before_capture` | 1877.0 | 406.7 | - |
| `before_exec_check` | 619.1 | 141.3 | - |
| `exec_to_ready` vs boot deadline **120 s** | 468.4 | 109.3 | 119.53 s / 119.89 s, successor side only (see below) |
| `exec_to_claim` vs lease + grace **15 s** | 407 | 86 | 14.59 s / 14.91 s; 13.59 s / 13.91 s if the last old heartbeat was one `heartbeat_ms` earlier |
| one hash pass `H` | 317.8 | 68.0 | - |
| `cold_spawn_to_ready` (reference) | 436.5 | 183.1 | - |

Reading the margins:

- The 5 s probe deadline covers one `--describe-handoff` run, which hashes
  the whole ELF once in the product. Measured on a warm page cache (not
  dropped). The run is I/O-bound by the read of the ELF: the debug binary is 569
  MB, so the **extrapolation** (not measured) is that a read below roughly
  114 MB/s would exceed 5 s for the debug binary and below roughly 25 MB/s for
  the release binary. A cold cache on slow storage is therefore the real
  risk to the unchanged probe deadline; this fixture cannot show it because
  it does not drop the host page cache.
- `exec_to_claim` is an upper bound of the claim instant (derived from the
  claimed lease; `claim_stable=true` in every sample: no heartbeat moved it).
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
  preflight ~2.51 s, re-probe ~2.19 s, before_exec ~0.94 s; for release:
  ~0.54 s, ~0.47 s, ~0.21 s. None of the derived values reaches 5 s, but
  the 5 s limit applies to the describe run only, not to these spans (the
  largest, the debug preflight at ~2.5 s, is about half of it).

No number was used to extend a deadline, and no latency is asserted in any test.

## What ran, and what waits for 5a

Executed here (all with both artifacts; `fixture` unless noted):

- Artifact: absent manifest, empty/blank variable, `CARGO_BIN_EXE_*` present
  but manifest absent (the fixture's own `require()` panics in a child
  process), size/hash mismatch, a manifest that resolves to the harness build,
  feature/stripped/profile/argument rules, modification after open.
- Supervision: descendants found and killed when the owner is dropped; clean
  exit with no orphans; an orphan left by the child is reported then reaped
  (the test process is a child subreaper, so the join is a kernel wait, not a
  `/proc` poll); exit before Ready is an event carrying stderr; the isolated
  directories (config, state, cache, home, tmp, project, runtime sockets) are
  removed.
- Product: starts through `--config-dir` in the isolated environment; PID and
  image (same device/inode as the manifest artifact); build identity
  (manifest hash = `--describe-handoff` identity); epoch and core identity
  from the handshake; descendants; descriptors (no inherited descriptor above
  2 without close-on-exec); a chat over the public wire and the exact
  provider request seen by the mock; SIGTERM exit; start failure with a bad
  option leaves nothing.
- Renderer helper (fake helper of `renderer-test-helper`, a harness binary):
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
- CI wiring of the artifact variables (provisional names in
  `shared-test-support/product_exec/provisional.rs`, to be replaced from I0).

## Reproduce

Build the artifact copies with the arguments in the table (build the test
binary first, then the product, then run the ignored
`write_manifest_for_built_artifact` with `EVORCH_F4P_BUILD_ELF`,
`EVORCH_F4P_BUILD_PROFILE`, `EVORCH_F4P_BUILD_ARGS`,
`EVORCH_F4P_ARTIFACT_DIR`), then:

```sh
EVORCH_F4P_PRODUCT_MANIFEST=<artifact dir>/manifest.json \
EVORCH_F4P_MEASURE_OUT=crates/evorch-core/measurements/f4-product \
EVORCH_F4P_MEASURE_LABEL=limited \
scripts/reproduce-under-limits.sh --cpus 4 --memory 8G --tasks 4096 --timeout 3000 -- \
  <f4_product_fixture test binary> --ignored --exact measure_f4_product_intervals --nocapture
```

(`loaded`: add `--stress-cpus 3` and label `loaded`.) The measurement ran
the test binary directly (not under `cargo`), so that a Cargo rebuild cannot
replace the artifact.
