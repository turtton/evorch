# M2 production composition evidence (4b-D-2c-3)

Restart is still an internal coordinator, not a public capability. M2 stage 12
(raw exec), CLI continuity and successor dispatch remain disconnected.

## Ownership and completion

The cold-core fixture calls `BootstrappedHost::open_for_core_with_test_seams`
with the production `CoreGraphRecovery` hook. Only the model and sandbox factory
are substituted: recorder, queue budgets, SQLite writer, journal/capture,
background services, owner worker, Unix control connections, HTTP/SSE and
backend delivery use their real implementations.

M2 stage 5 is **not a SQLite writer join**:

1. `flush()` obtains a FIFO ACK for the accepted queue and a finite bus snapshot.
2. `try_shutdown()` stops the subscriber, drains its final snapshot, closes the
   recorder queue, waits for the bridge persistence worker's final usage flush,
   and joins the subscriber OS thread.
3. A separate SQLite writer barrier completes.
4. The bridge still owns `Storage`. The SQLite writer stays alive for owner-hold
   recording, Arm, readback and FailedClosed invalidation.
5. Only stages 9–11 prepare FDs, transfer the signal reader, then stop/join the
   owner. Fail-closed diagnostic restoration is not business resumption.

The final usage flush is measured separately from pushing the last queue items.
The core bootstrap's ordinary usage tick is 5 seconds; neither that interval nor
a flush ACK is a drain/join receipt. No new bridge deadline is introduced.
The existing whole-cohort 10s, reap 1s and worker-join 1s limits are unchanged.

## Tests and observation

`tests/finalization/later/last/composition.rs` reuses the real stages 9–11
subprocess/lifecycle fixture. Its tracing observer holds independently owned
blocking workers with channels, never with sleeps. Web completion milestones use
an injected per-instance observer behind `workbench-web/test-support`, enabled
by `evorch-core/bootstrap-test-support` (the existing runtime CI feature).
The default production observer is a no-op; no dependencies or boundary
allowlists change. The fixture observes the empty queue
through a completed sentinel write, then exercises 0/100/16383/16385 workload
events against the production 16384-event budget and 4096-slot event bus.
The bus is paced independently so backpressure is recorder backpressure, not
accidental bus lag. Each cell also has 0 or 10000 seeded SQLite events.

At the real flush ACK, the test holds the caller and proves that stage 5 has not
completed and the writer can still accept a barrier. An additional internal
audit event then proves the final snapshot is distinct from that ACK. Independent
SQL readback checks every workload event exactly once. All three runs belonging
to one owner/generation retain their captured snapshot receipts and parked-run
inventory. All successful drain cells intentionally end FailedClosed at the
disconnected raw-exec boundary.

Failure cells drop the real ACK sender by panicking its worker, panic the real
subscriber OS thread after its persistence worker has joined, fail a real
background join, or reject invalidation using a SQLite trigger. Cleanup remains
owned, errors stay sticky, uncertain invalidation is retried, owner holds remain
until invalidation is confirmed, and no business gate is reopened.

Existing capture/Arm/owner/held-Web race matrices use the same production
cold-core startup. They inject Stop and shutdown on both sides of capture,
after Arm, and around owner stop. The existing injected-clock deadline tests
remain the timing-boundary evidence; real `Instant` values in the composition
observer are only performance reports, never pass/fail thresholds.

See `../../measurements/4b-d-2c-3/README.md` for the reproducible measurement
command, environment, raw results, failure results and D-3 handoff.

The existing final-business/usage durability test also uses production cold-core
bootstrap: independent DB reads verify the final event, per-request usage and
aggregated usage bucket, in addition to the repeated queue-order measurements.
