# 4b-D-2c-3: production-bootstrap M2 composition

## Method and environment

- Base implementation: main `0b9011c71baaf50ff3d067c242d60ac42445a8e8` (#637, including #636).
- Linux 7.3.1-cachyos, x86_64, AMD Ryzen 9 7900 (12 cores / 24 logical CPUs), local NVMe/ext4; fixture DBs in /tmp on the same ext4 filesystem as the worktree.
- rustc 1.99.0 (b940084d7), turtton/cargo 0.102.0-9-turtton.4 (cargo 1.101.0); unoptimized test profile with debuginfo, Nix dev environment. Instrumented debug numbers, not release/remote-disk estimates.
- One matrix parent at a time, no concurrent Cargo build/test started by this task during measurement, no synthetic CPU/I/O stress. Host-wide contention is not controlled. Recorder backpressure is deterministically induced with a held real persistence worker, not a simulated negative ACK.
- 10 fresh-DB samples per queue/DB pair, 8 pairs, 80 stage-11 cells, 1040 operation durations. All reach the intentionally disconnected raw-exec boundary and end FailedClosed; these are **not successful execs**. Zero unexpected measurement failures. No latency-based functional assertions.
- Quantiles use nearest rank: sorted sample at `ceil(p*n)-1`. With n=10, p95 equals max; this small sample does not characterize fleet tail latency. Raw observations: [samples.csv](samples.csv), [loads.csv](loads.csv). No outliers or warmups discarded.

The fixture uses `open_for_core_with_test_seams` and `CoreGraphRecovery`; only model and sandbox factories are substituted. Recorder/subscriber, SQLite writer, capture journal, background services, owner, core backend, Web server, Unix control/FD preparation and signal-reader stop are real. Three parked runs share one owner/generation. Independent SQL readback checks the complete accepted workload (including an event emitted after flush ACK), each snapshot receipt, and the saved parked-run inventory.

A completed sentinel write establishes the empty-queue baseline before loading. Workload counts are 0, 100, 16383 and 16385 uncoalesced events. The production event budget is 16384 and the bus is 4096 slots; 512-event pacing avoids accidental bus loss. The 16385 profile has 16384 admitted and one blocked by actual backpressure. `loads.csv` also records the observed queue size at flush entry. DB profiles add 0 or 10000 valid historical event rows to the production schema; bootstrap contributes another 28 rows before load. Logical size is `page_count * page_size`, not a sum of DB/WAL files.

## Reproduce

Run in the Nix dev environment (or the repository sandbox-build environment wrapper). The existing runtime CI test group already enables bootstrap-test-support; no central CI change is needed.

```sh
EVORCH_M2_SAMPLES=10 cargo nextest run --offline -p evorch-core --lib \
  --features bootstrap-test-support \
  -E 'test(production_composition) & not test(uncertain_ack)' \
  --test-threads 1 --success-output immediate
cargo nextest run --offline -p evorch-core --lib \
  --features bootstrap-test-support -E 'test(production_composition_uncertain_ack)' \
  --success-output immediate
```

Read `M2-MEASURE queue,seeded_rows,operation,milliseconds`, `M2-SIZE queue,seeded_rows,actual_rows,logical_bytes` and `M2-DEPTH queue,seeded_rows,pending_at_flush` from the successful subprocess output. CSV sample numbers are encounter order within each queue/DB/operation. The normal functional suite uses one sample per pair; repeats do not change any production/test deadline.

## Intervals (do not sum nested spans)

- `flush_ack`: real FIFO flush request through receipt, including draining the held queue. ACK alone proves neither subscriber nor writer termination.
- `subscriber_drain_join`: before subscriber stop is sent through OS-thread join; includes its final finite snapshot, queue closure, final usage flush and bridge-worker completion.
- `final_flush`: actual final usage flush inside the persistence worker after request-channel closure, **not** the earlier coalescing-queue flush.
- `bridge_worker_join`: final flush completion through the subscriber's successful await of the persistence worker.
- `writer_barrier`: the separate SQLite writer barrier **after** subscriber join. SQLite writer remains alive for Arm/readback/FailedClosed; there is no SQLite writer join measurement.
- `web_connections`: stop listener/connections through connection join; `web_api_sse`: subsequent detached API/SSE task-registry join; `web_backend`: actual backend-delivery drain.
- `web_api_request`: a real status API task's lifetime before restart; `web_sse_stop`: connection-stop initiation through the live SSE pump's completion. The existing held-Web matrix separately proves live API/backend work blocks convergence, including virtual-clock deadline and shutdown interruptions.
- Web milestones use an injected per-instance observer behind `workbench-web/test-support`, default no-op; no tracing dependency or allowlist extension. Other milestones use existing tracing observations. Timestamps are taken only by test observers.
- `fd_prepare`: prepared FD/ticket/argv handoff construction; earlier target verification/hash/bootstrap is outside this interval. `signal_transfer`: stopping/joining the signal reader into its stopped transfer object (not successor adoption). `owner_stop_join`: owner worker stop/join; its end is observed only on the already-latched FailedClosed branch, never in a future raw-exec success window.

## Flush overview

| Workload events | Seeded DB events | n | Flush ACK p50 / p95 / max (ms) | Unexpected failures |
|---:|---:|---:|---:|---:|
| 0 | 0 | 10 | 0.083 / 0.104 / 0.104 | 0 |
| 0 | 10000 | 10 | 0.084 / 0.100 / 0.100 | 0 |
| 100 | 0 | 10 | 16.203 / 17.096 / 17.096 | 0 |
| 100 | 10000 | 10 | 15.899 / 22.894 / 22.894 | 0 |
| 16383 | 0 | 10 | 2682.363 / 3805.400 / 3805.400 | 0 |
| 16383 | 10000 | 10 | 2817.510 / 3453.849 / 3453.849 | 0 |
| 16385 | 0 | 10 | 2608.934 / 3371.874 / 3371.874 | 0 |
| 16385 | 10000 | 10 | 2616.813 / 3231.001 / 3231.001 | 0 |

## Full operation results

All rows use the environment above, n=10 and zero unexpected failures.

### Queue workload 0, seeded DB events 0

Actual pre-load event rows: 28; logical SQLite bytes: 1564672; pending at flush entry: 0.

| Operation | n | p50 ms | p95 ms | max ms | Unexpected failures |
|---|---:|---:|---:|---:|---:|
| flush_ack | 10 | 0.083 | 0.104 | 0.104 | 0 |
| subscriber_drain_join | 10 | 0.805 | 0.946 | 0.946 | 0 |
| final_flush | 10 | 0.001 | 0.001 | 0.001 | 0 |
| bridge_worker_join | 10 | 0.038 | 0.057 | 0.057 | 0 |
| writer_barrier | 10 | 0.754 | 0.917 | 0.917 | 0 |
| web_connections | 10 | 0.268 | 0.297 | 0.297 | 0 |
| web_api_sse | 10 | 0.002 | 0.002 | 0.002 | 0 |
| web_api_request | 10 | 0.257 | 0.285 | 0.285 | 0 |
| web_sse_stop | 10 | 0.039 | 0.088 | 0.088 | 0 |
| web_backend | 10 | 0.001 | 0.002 | 0.002 | 0 |
| fd_prepare | 10 | 0.747 | 0.791 | 0.791 | 0 |
| signal_transfer | 10 | 0.002 | 0.003 | 0.003 | 0 |
| owner_stop_join | 10 | 0.168 | 0.209 | 0.209 | 0 |

### Queue workload 0, seeded DB events 10000

Actual pre-load event rows: 10028; logical SQLite bytes: 3624960; pending at flush entry: 0.

| Operation | n | p50 ms | p95 ms | max ms | Unexpected failures |
|---|---:|---:|---:|---:|---:|
| flush_ack | 10 | 0.084 | 0.100 | 0.100 | 0 |
| subscriber_drain_join | 10 | 0.812 | 1.114 | 1.114 | 0 |
| final_flush | 10 | 0.001 | 0.001 | 0.001 | 0 |
| bridge_worker_join | 10 | 0.035 | 0.050 | 0.050 | 0 |
| writer_barrier | 10 | 0.622 | 0.905 | 0.905 | 0 |
| web_connections | 10 | 0.264 | 0.297 | 0.297 | 0 |
| web_api_sse | 10 | 0.002 | 0.002 | 0.002 | 0 |
| web_api_request | 10 | 0.257 | 0.275 | 0.275 | 0 |
| web_sse_stop | 10 | 0.043 | 0.091 | 0.091 | 0 |
| web_backend | 10 | 0.001 | 0.001 | 0.001 | 0 |
| fd_prepare | 10 | 0.767 | 0.823 | 0.823 | 0 |
| signal_transfer | 10 | 0.002 | 0.003 | 0.003 | 0 |
| owner_stop_join | 10 | 0.178 | 0.212 | 0.212 | 0 |

### Queue workload 100, seeded DB events 0

Actual pre-load event rows: 28; logical SQLite bytes: 1564672; pending at flush entry: 100.

| Operation | n | p50 ms | p95 ms | max ms | Unexpected failures |
|---|---:|---:|---:|---:|---:|
| flush_ack | 10 | 16.203 | 17.096 | 17.096 | 0 |
| subscriber_drain_join | 10 | 0.829 | 4.294 | 4.294 | 0 |
| final_flush | 10 | 0.001 | 0.002 | 0.002 | 0 |
| bridge_worker_join | 10 | 0.040 | 0.056 | 0.056 | 0 |
| writer_barrier | 10 | 0.671 | 6.143 | 6.143 | 0 |
| web_connections | 10 | 0.258 | 0.308 | 0.308 | 0 |
| web_api_sse | 10 | 0.002 | 0.002 | 0.002 | 0 |
| web_api_request | 10 | 0.261 | 0.325 | 0.325 | 0 |
| web_sse_stop | 10 | 0.040 | 0.090 | 0.090 | 0 |
| web_backend | 10 | 0.001 | 0.002 | 0.002 | 0 |
| fd_prepare | 10 | 0.774 | 0.818 | 0.818 | 0 |
| signal_transfer | 10 | 0.003 | 0.003 | 0.003 | 0 |
| owner_stop_join | 10 | 0.163 | 0.208 | 0.208 | 0 |

### Queue workload 100, seeded DB events 10000

Actual pre-load event rows: 10028; logical SQLite bytes: 3624960; pending at flush entry: 100.

| Operation | n | p50 ms | p95 ms | max ms | Unexpected failures |
|---|---:|---:|---:|---:|---:|
| flush_ack | 10 | 15.899 | 22.894 | 22.894 | 0 |
| subscriber_drain_join | 10 | 0.875 | 3.282 | 3.282 | 0 |
| final_flush | 10 | 0.001 | 0.002 | 0.002 | 0 |
| bridge_worker_join | 10 | 0.042 | 0.068 | 0.068 | 0 |
| writer_barrier | 10 | 0.832 | 4.934 | 4.934 | 0 |
| web_connections | 10 | 0.274 | 0.349 | 0.349 | 0 |
| web_api_sse | 10 | 0.002 | 0.003 | 0.003 | 0 |
| web_api_request | 10 | 0.273 | 0.415 | 0.415 | 0 |
| web_sse_stop | 10 | 0.083 | 0.088 | 0.088 | 0 |
| web_backend | 10 | 0.001 | 0.002 | 0.002 | 0 |
| fd_prepare | 10 | 0.820 | 0.860 | 0.860 | 0 |
| signal_transfer | 10 | 0.002 | 0.003 | 0.003 | 0 |
| owner_stop_join | 10 | 0.185 | 0.215 | 0.215 | 0 |

### Queue workload 16383, seeded DB events 0

Actual pre-load event rows: 28; logical SQLite bytes: 1564672; pending at flush entry: 16383.

| Operation | n | p50 ms | p95 ms | max ms | Unexpected failures |
|---|---:|---:|---:|---:|---:|
| flush_ack | 10 | 2682.363 | 3805.400 | 3805.400 | 0 |
| subscriber_drain_join | 10 | 0.820 | 1.720 | 1.720 | 0 |
| final_flush | 10 | 0.001 | 0.001 | 0.001 | 0 |
| bridge_worker_join | 10 | 0.040 | 0.063 | 0.063 | 0 |
| writer_barrier | 10 | 0.678 | 1.288 | 1.288 | 0 |
| web_connections | 10 | 0.279 | 0.327 | 0.327 | 0 |
| web_api_sse | 10 | 0.002 | 0.002 | 0.002 | 0 |
| web_api_request | 10 | 0.257 | 0.280 | 0.280 | 0 |
| web_sse_stop | 10 | 0.066 | 0.108 | 0.108 | 0 |
| web_backend | 10 | 0.001 | 0.002 | 0.002 | 0 |
| fd_prepare | 10 | 0.773 | 1.159 | 1.159 | 0 |
| signal_transfer | 10 | 0.003 | 0.008 | 0.008 | 0 |
| owner_stop_join | 10 | 0.182 | 0.710 | 0.710 | 0 |

### Queue workload 16383, seeded DB events 10000

Actual pre-load event rows: 10028; logical SQLite bytes: 3624960; pending at flush entry: 16383.

| Operation | n | p50 ms | p95 ms | max ms | Unexpected failures |
|---|---:|---:|---:|---:|---:|
| flush_ack | 10 | 2817.510 | 3453.849 | 3453.849 | 0 |
| subscriber_drain_join | 10 | 0.876 | 2.278 | 2.278 | 0 |
| final_flush | 10 | 0.001 | 0.002 | 0.002 | 0 |
| bridge_worker_join | 10 | 0.043 | 0.062 | 0.062 | 0 |
| writer_barrier | 10 | 0.777 | 0.848 | 0.848 | 0 |
| web_connections | 10 | 0.291 | 0.532 | 0.532 | 0 |
| web_api_sse | 10 | 0.002 | 0.002 | 0.002 | 0 |
| web_api_request | 10 | 0.272 | 0.457 | 0.457 | 0 |
| web_sse_stop | 10 | 0.063 | 0.107 | 0.107 | 0 |
| web_backend | 10 | 0.001 | 0.002 | 0.002 | 0 |
| fd_prepare | 10 | 0.818 | 0.872 | 0.872 | 0 |
| signal_transfer | 10 | 0.003 | 0.004 | 0.004 | 0 |
| owner_stop_join | 10 | 0.191 | 0.381 | 0.381 | 0 |

### Queue workload 16385, seeded DB events 0

Actual pre-load event rows: 28; logical SQLite bytes: 1564672; pending at flush entry: 16384.

| Operation | n | p50 ms | p95 ms | max ms | Unexpected failures |
|---|---:|---:|---:|---:|---:|
| flush_ack | 10 | 2608.934 | 3371.874 | 3371.874 | 0 |
| subscriber_drain_join | 10 | 0.795 | 1.750 | 1.750 | 0 |
| final_flush | 10 | 0.001 | 0.001 | 0.001 | 0 |
| bridge_worker_join | 10 | 0.036 | 0.055 | 0.055 | 0 |
| writer_barrier | 10 | 0.611 | 0.815 | 0.815 | 0 |
| web_connections | 10 | 0.280 | 0.559 | 0.559 | 0 |
| web_api_sse | 10 | 0.002 | 0.002 | 0.002 | 0 |
| web_api_request | 10 | 0.252 | 0.311 | 0.311 | 0 |
| web_sse_stop | 10 | 0.070 | 0.359 | 0.359 | 0 |
| web_backend | 10 | 0.001 | 0.002 | 0.002 | 0 |
| fd_prepare | 10 | 0.734 | 0.943 | 0.943 | 0 |
| signal_transfer | 10 | 0.002 | 0.003 | 0.003 | 0 |
| owner_stop_join | 10 | 0.159 | 0.204 | 0.204 | 0 |

### Queue workload 16385, seeded DB events 10000

Actual pre-load event rows: 10028; logical SQLite bytes: 3624960; pending at flush entry: 16384.

| Operation | n | p50 ms | p95 ms | max ms | Unexpected failures |
|---|---:|---:|---:|---:|---:|
| flush_ack | 10 | 2616.813 | 3231.001 | 3231.001 | 0 |
| subscriber_drain_join | 10 | 0.803 | 0.898 | 0.898 | 0 |
| final_flush | 10 | 0.001 | 0.001 | 0.001 | 0 |
| bridge_worker_join | 10 | 0.041 | 0.063 | 0.063 | 0 |
| writer_barrier | 10 | 0.637 | 0.784 | 0.784 | 0 |
| web_connections | 10 | 0.280 | 0.310 | 0.310 | 0 |
| web_api_sse | 10 | 0.002 | 0.002 | 0.002 | 0 |
| web_api_request | 10 | 0.253 | 0.265 | 0.265 | 0 |
| web_sse_stop | 10 | 0.060 | 0.090 | 0.090 | 0 |
| web_backend | 10 | 0.001 | 0.002 | 0.002 | 0 |
| fd_prepare | 10 | 0.745 | 0.782 | 0.782 | 0 |
| signal_transfer | 10 | 0.002 | 0.003 | 0.003 | 0 |
| owner_stop_join | 10 | 0.159 | 0.188 | 0.188 | 0 |

## Injected failure results

Same environment, production bootstrap, three parked runs, bootstrap-only DB, no extra queue workload; one case each. These are functional cleanup probes, not latency samples: p50/p95/max are N/A because the requested success endpoint intentionally does not exist.

| Failure | n | Last completed stage | Result |
|---|---:|---|---|
| Real ACK sender lost before reply | 1 | WorkersJoined | FailedClosed; subscriber reclaimed despite unknown ACK; writer retained; no owner stop/business reopening |
| Real subscriber OS-thread panic after bridge worker joined | 1 | WorkersJoined | Successful ACK cannot advance to writer barrier/owner stop; retained cleanup reports sticky join error |
| Background task join panic | 1 | ChildrenReaped | No later stage/owner stop; cleanup remains owned and sticky |
| SQLite trigger rejects invalidation | 1 | Armed | Hold retained until confirmed invalidation retry; writer and all snapshot receipts retained; business remains closed |

All four cases verify every expected persisted snapshot. Early M2 failures precede the stage-6 owner-progress receipt, so only the post-Arm case additionally checks that receipt's parked-run inventory. Removing the invalidation trigger allows cleanup to release the hold only after confirmed invalidation; the original error remains sticky.

During development, the first draft used an invalid SSE scope and incorrectly expected cleanup's sticky error to clear after retry; both fixture assertions were corrected. A later shared checkpoint helper incorrectly required the not-yet-captured stage-6 owner-progress receipt on an early failure; it now always checks snapshots and checks owner progress only after capture. These failed development runs are not silently mixed into the successful repeated-measurement population. The attempted Web tracing dependency was removed when the unchanged boundary gate rejected it; only test-feature definitions remain.

## D-3 handoff / limitations

- The observed queue → subscriber → SQLite barrier → owner order, finite post-ACK snapshot and three same-owner checkpoints are now composition-level evidence, not only stage mocks. Keep those checks when adding the real exec call.
- Queue size dominates flush ACK (up to 3805.400 ms here). This instrumented, healthy local-disk sample is **not** evidence to extend the existing whole-cohort 10s, reap 1s or join 1s budgets. They remain unchanged; no new bridge deadline was selected. Slower/loaded disk/CPU, much larger DBs and fleet-tail behavior remain unverified.
- Preserve the currently live SQLite writer for Arm, independent readback and FailedClosed cleanup. A flush ACK must never be promoted to a writer-termination proof.
- Preserve ownership of failed joins/uncertain persistence and retryable invalidation; do not reopen business or abandon handles. Existing capture/Arm/owner-stop Stop/shutdown matrices and injected-clock boundaries now use cold-core startup as well.
- D-3 still must connect only the prepared raw call after the pending-signal/latch checks, with no await/DB/allocation-heavy observer in that success window; keep failed-exec diagnostic restoration separate. Actual exec/successor signal adoption, public restart capability and CLI continuity are deliberately unimplemented here.

The existing final-business/usage durability test also uses production cold-core
bootstrap: independent DB reads verify the final event, per-request usage and
aggregated usage bucket, in addition to the repeated queue-order measurements.

## Local validation

- Related suite: `cargo nextest run --offline -p evorch-core -p recorder -p workbench-web -p workbench-host --features evorch-core/bootstrap-test-support`: 570 passed, 4 skipped (ignored/helper or environment-specific cases).
- Final-business/request/aggregate usage durability test rerun after upgrading its bootstrap: 1 passed.
- Repeated production composition: 4 parent tests passed, 80 cells / 1040 durations, no unexpected failures; cleanup matrix: 1 parent test passed, all 4 injected failure cases confirmed.
- `cargo clippy --offline --workspace --all-targets -- -D warnings` and `cargo clippy --offline -p evorch-core --all-targets --features bootstrap-test-support -- -D warnings`: passed.
- `cargo fmt --all -- --check`, workbench dependency boundaries, core-client reference baseline, integration scope and staged whitespace checks: passed. The only central differences are the explicitly permitted test-feature definitions; Cargo.lock and dependency allowlists are unchanged.
- Cache-contract script not rerun locally: no provider input/history, schema, prompt, model setting or cache-affinity behavior changes. Existing cache tests are left intact and run in normal CI. CI status is reported in the PR, not inferred from local results.
