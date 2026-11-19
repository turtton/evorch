# M2 production composition evidence (4b-D-2c-3)

Restart is still an internal coordinator, not a public capability. M2 stage 12
(raw exec) and internal product successor dispatch are now connected (D-3b); public restart commands and CLI continuity remain deferred to 5a/5b.

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
a flush ACK is a drain/join receipt. The existing whole-cohort 10s, reap 1s and
worker-join 1s limits are unchanged.

## Storage drain budget (4b-D-3a)

Stage 5 has one **10s budget**, shared by the FIFO flush ACK, subscriber stop /
final snapshot / final usage flush / bridge-worker and subscriber joins, and the
separate SQLite writer barrier. It is not renewed between those operations. It
is separate from Preparing's cohort budget and from the fixed 120s successor
boot deadline, which has not started at stage 5.

D-2c-3 measured a maximum healthy local flush ACK of 3805.400ms (about 3.8s)
across 80 cells; the other drain intervals were much smaller. A 1s or 5s budget
would respectively exclude this observed workload or leave little headroom.
10s provides about 2.6x that maximum without extending any existing budget.
This is a conservative fail-closed policy choice, **not** a fleet-tail or slow
disk guarantee. Larger databases / slower disks may exceed it and must not exec.

On expiry, only the borrowed wait is cancelled. The shared `PlannedExit` keeps
the exact join handle, whose task still owns the bridge and storage. Neither
abort, detach, owner stop/release nor business resumption is allowed. The
coordinator invalidates the same captured transaction and publishes FailedClosed;
its independent cleanup and operator shutdown await the retained task to actual
completion, without minting a new M2 budget. A late success cannot complete stage
5 or retry M2. An unknown/failed flush ACK still leads to subscriber/bridge-worker
reclamation within that same task; its failure remains sticky. This deadline
bounds restart progress, **not** total process exit or uninterruptible storage
I/O. The SQLite writer stays alive for diagnostic/invalidation readback.

The production composition deadline matrix uses channels to hold real flush,
final-usage-flush and writer-barrier workers plus explicit virtual-clock advances.
Its eight cells cover expiry at each point, late unknown ACK cleanup, a shared
4s + 4s + 2s budget, completion released at 9.999s, and operator shutdown while
the task is held both before and after expiry. Expiry advances across Tokio's next millisecond timer tick;
no wall-clock wait or yield loop drives the test. It independently reads three
same-owner parked checkpoints in the populated-core cells. An explicit tracing
checkpoint after runtime drain proves that shutdown is waiting for planned
cleanup while the owner row, generation and scratch marker are still retained.
The pre-expiry cell has no live run (one completed turn creates real identified
scratch) and reaches the releasable idle-owner path: channel release permits
cleanup to finish before ownership/scratch release. The
post-expiry populated-core cell keeps the existing failed durable-dispatch fence,
so ownership/scratch remain retained even after cleanup. Neither path changes the
captured manifest or reopens M2/business.
Existing composition workload and failure tests remain: they protect different
contracts (actual durability, load/backpressure, panics and invalidation retry).

### Explicit shutdown ordering and remaining exit bound

`take_planned_exit` moves the bridge out of `BootstrappedHost`, so the ordinary
`self.bridge.flush()` branch is skipped. Shutdown must therefore await planned
cleanup **before** `owner.release_ready()` (which can also delete scratch), not
only after releasing ownership. The narrowly scoped central bootstrap change
reorders that operation and calls a private planned-exit helper; its tracing
events observe progress only and grant no production authority.

Explicit shutdown while drain is pending still waits for actual cleanup **with
no upper bound**, as it did before this ordering fix. D-3a bounds M2 stage 5,
not shutdown or Tokio runtime destruction. A follow-up **D-3a2** PR must design
and independently review bounded process termination: a planned-shutdown-only
independent watchdog with a separate 10s budget; expiry must leave owner rows
untouched and end the process without running Drop, allowing leases to expire.
An async timeout returning an error is insufficient because runtime destruction
can still join outstanding blocking work, and synchronous DB calls can stall
the executor. The termination primitive, ordinary-shutdown interaction and
channel/injected-clock subprocess proof belong to that follow-up, not this PR.

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
inventory. Drain-only cells deliberately remove their fixture entry after owner stop and
exercise real execve failure/FD rollback. Product-success cells are separate.

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

## D-3b: internal product exec/bootstrap connection

After stage 11, the successful tail holds synchronous lifecycle/runtime
execution-prohibition latches, checks the stopped signalfd without consuming it,
and calls the prepared raw syscall boundary. It does not await, access SQLite,
allocate/format strings, notify or log. Only a failed return releases the
latches, restores FD flags/diagnostic ownership and invalidates the same TX;
old business never resumes. A successful exec replaces the process image while
retaining the lifetime-lock OFD and one-use ticket. Product entry classifies
planned mode and inspects inherited descriptors before creating an inbox,
runtime or database; failed validation never falls back to Cold.

The hidden read-only `--describe-handoff` preflight is not a protocol capability
or a user restart operation. It opens no DB/owner/lease/lock and starts no
resident service. Its storage range comes from the migration registry and
all canonical current-format gates; protocol comes from ProtocolVersion::CURRENT. Its
standard-tools-v1 identity covers only the eight compiled standard tools:
read, edit, write, apply_patch, grep, shell, git_diff and run_js, in the same
name-sorted metadata order as actual standard registration. A parity test
compares names, descriptions, schemas and order without changing registration.
Optional network/web/browser tools, host and runtime meta tools, configuration,
MCP and skills are **not** included in this preflight catalog identity. It is
neither an effective per-run catalog nor permission to change the prompt.

Per-incarnation ToolSnapshot is the boundary authority. M2 snapshots the actual
sent ProviderInput before Commit, selecting builtins using
metadata.old_builtin_tool_names. Planned recovery compares the current builtin
preview with this evidence after Ready but before FULL activation ACK. The
pure composition helper is shared with the live loop. Saved FrozenMcpSnapshot
metadata is restored through the same inert registration API and name-permission
helper as the real loop, using a disposable bus/executor: no discovery, credentials,
server process or connection. An absent snapshot never invents external tools.
External tool definitions remain unchanged and do not acquire a build-change grant. Non-tool context,
execution authority, history, ToolResults and affinity must remain identical.
Only a verified different build with changed builtin schema/description/order
may consume one tools-only boundary; same build and unchanged catalogs grant
none. Missing or unexplainable builtin evidence fails closed, never grants an
unverified boundary or rewrites sent history.

Public restart/quarantine operations and capability publication, client
reconnection and CLI continuity remain 5a/5b work. The explicit-shutdown watchdog
remains D-3a2 and is not added here.
