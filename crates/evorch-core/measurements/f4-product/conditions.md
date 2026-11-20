# F4-P measurement conditions (fixed before the first run)

This file is committed **before** any measurement run. Results are in the
files next to it; they must be read against these conditions, which were not
adjusted after seeing numbers.

## What is measured

The product is the ordinary `evorch-core` entry, built separately without
features with the shipped arguments:

| profile | Cargo arguments |
|---|---|
| debug   | `cargo build -p evorch-core --bin evorch-core` (repository `[profile.dev]`: `debug = "line-tables-only"`, dependencies without debuginfo, `sha2` at `opt-level = 3`) |
| release | `cargo build --release -p evorch-core --bin evorch-core` (Cargo default release profile; the crane `evorch-core` package passes the same `-p evorch-core --bin evorch-core`) |

Nothing is stripped. Each artifact is a byte-identical copy of the Cargo
output together with a manifest (feature set, rustc/cargo, ELF size, SHA-256).
The CI main job's `CARGO_PROFILE_DEV_DEBUG=0` is **not** measured here.

## Intervals

| id | what runs | where the clock starts / stops |
|---|---|---|
| `preflight` | `RestartTarget::preflight` for target id `target` (not `current`): launcher hashing of the entry and ELF, the `--describe-handoff` probe run, requirement checks | call → return |
| `reprobe_before_capture` | `RestartTarget::before_capture` (same probe again, equality check) | call → return |
| `before_exec_check` | `CaptureCheckedTarget::before_exec` (revalidation of the entry and ELF fingerprints) | call → return |
| `describe_probe_run` | one `evorch-core --describe-handoff` run: the work under the 5 s probe deadline | spawn → output complete and exited |
| `hash_pass` | one SHA-256 pass over the ELF (the unit cost `H` of every fingerprint) | open → digest |
| `cold_spawn_to_ready` | product spawned through its ordinary entry in a fresh isolated environment, until its readiness line | spawn call returns → line received by the reader thread |
| `exec_to_ready` | real `execve` of the harness old-core stand-in into the product, until the readiness line | pipe EOF (close-on-exec ends closed by the kernel during `execve`) → readiness line |
| `exec_to_claim` | same exec, until the owner claim the successor wrote | pipe EOF (wall clock) → `lease.expires_at - lease_ms` of the claimed owner (an upper bound of the claim instant) |

`ProbeLimits::default()` (5 s), the 300 s target timeout, the 120 s boot
deadline and the 5 s lease + 10 s grace (15 s) are the production values and
are not changed. A probe that exceeds its limit is recorded as a failed sample,
never retried with a longer limit. The watchdogs of the fixture itself (120 s
to Ready, 60 s to exit) only turn a hang into a failure.

## Environment

- Linux 7.3.1-cachyos x86_64, AMD Ryzen 9 7900 (12 cores / 24 threads), 64 GiB,
  local NVMe ext4 (`/var/tmp` and the Cargo target pool on the same filesystem),
  `performance` governor.
- rustc 1.99.0, turtton/cargo 0.102.0-9-turtton.4, Nix dev shell.
- The host page cache is **not** dropped. Copies are written and hashed by the
  fixture's verification before use, so a "first" sample measures the first use
  of a new inode and process image, not storage-cold reads.
- One measurement process at a time; no Cargo build runs during a measurement.
- No test deadline is changed; no latency is asserted by the measurement tests.

### Resource conditions

Both run through `scripts/reproduce-under-limits.sh`, which applies the limits
to the measurement process and every descendant inside one systemd scope:

| condition | limits | load |
|---|---|---|
| `limited` | 4 CPUs (CPUQuota 400%), memory 8 GiB, tasks 4096 | none |
| `loaded`  | same | 3 niced CPU stressors **inside the same scope** (`--stress-cpus 3`) |

## Counts

- **first**: 5 samples. Each uses a fresh copy of the artifact (new inode, new
  path, new isolated environment). Within one fresh copy the operations run in
  the fixed order 1 `preflight` chain (preflight, re-probe, before_exec),
  2 `describe_probe_run`, 3 `hash_pass`, 4 `cold_spawn_to_ready`, 5 exec cycle
  (`exec_to_ready`, `exec_to_claim`); the `seq` column records the position.
- **warm**: 20 samples on one already used artifact copy (the manifest's own
  artifact, verified and probed before the series), same order, no sample
  discarded.
- Quantiles are nearest rank (`ceil(p*n)-1` of the sorted successful samples).
  With n = 5 p95 equals the maximum; with n = 20 it is the 19th value.

## Measurable now / not measurable now

Measurable at this stage (no 5a): the launcher's preflight, re-probe and
pre-exec checks against a product-sized target; the probe run; the hash unit
cost; the product's cold boot; and, from a **harness** old core, the exec to
Ready and owner claim of the successor.

Not measurable until 5a / F4-E: the public request path (`request` to
`preflight` in the coordinator, config reload), the `current`-target path
(`CurrentElf::revalidate` hashing the product's own ELF: the fixture can only
add `k * H` as a derived estimate, `k` counted from the launcher code, and
labels it as derived), the capture stages and `Restarting -> exec` of a
production old core, and the product -> product Ready inside the 120 s boot
deadline (which starts at `Restarting`, before exec preparation). The
harness-based `exec_to_ready` is the successor's side only: it is neither a
bound nor a substitute for those intervals.

## Addendum after rebasing onto I0 (#711, `219e3bfd`), before the rerun

The first runs (base `1ed6c863`) are superseded: every number in `README.md`
and the CSV files comes from the rerun described here. Counts, intervals,
resource conditions and the page-cache policy above are unchanged. What changed:

- **Base** `219e3bfd` (includes #713, #714).
- **Release artifact** is now the sealed copy of I0's contract: built and sealed
  by `scripts/product-artifact.sh build` (`cargo build --locked --release -p
  evorch-core --bin evorch-core`, read-only copy, manifest read through
  `EVORCH_PRODUCT_CORE_EXE` / `EVORCH_PRODUCT_MANIFEST`). This adds `--locked` and,
  in this environment, `RUSTFLAGS=-C link-args=-fuse-ld=mold` from the dev shell.
- **Debug artifact** is outside the sealed contract (which only covers release).
  It stays a byte-identical copy with a measurement-only manifest
  (`EVORCH_F4P_MEASURE_MANIFEST`), built with `cargo build -p evorch-core --bin
  evorch-core`. No product test reads that variable.

## Addendum 2 after review of #715 (base `8ec04796`, includes #716), before the second rerun

The numbers of the first rerun are superseded; counts, intervals and
resource conditions are unchanged. What changed:

- **Base** `8ec04796` (adds the planned-shutdown watchdog, #716), so both artifacts
  are rebuilt.
- **Fixture**: process supervision now signals through a pidfd, observes the
  exit without reaping, and reclaims by group, token and marker; descriptors
  without close-on-exec are closed in the child. These run around, not inside,
  the timed regions, and the tables are re-taken with them.
- **Meaning of the exec intervals**: the start is the instant the watcher
  thread had read the pipe EOF (an observation instant, later than the
  kernel's close by an unmeasured wake-up latency), so `exec_to_ready` and
  `exec_to_claim` are under-measured by that latency; neither is claimed to be
  a bound of the true interval. A negative interval is recorded as an invalid
  (failed) sample, not as 0, a failed cycle records both rows with its own
  duration, and the CSV is saved after every round.
