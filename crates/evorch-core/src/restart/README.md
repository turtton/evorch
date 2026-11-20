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

### D-3a2-A: Linux core watchdog (not the complete D-3a2 guarantee)

D-3a's stage-5 drain deadline does not bound shutdown or runtime destruction.
The independent OS watchdog adds a separate, fixed **10 seconds**, armed once
when both this incarnation's irreversible capture and explicit SIGINT/SIGTERM
have been observed, in either order. Cold and planned entry start ineligible;
request acceptance or successor origin alone never qualifies. Idle exit,
startup/listener errors, successful exec, and shutdown without capture do not
arm it. Non-Linux retains its previous behavior and has **no watchdog guarantee**.

Both product entries use the same outer runtime lifetime. The watchdog is
prepared after single-threaded entry/adoption, before Tokio, and is completed
only after runtime destruction. It observes a CLOEXEC duplicate of the existing
signal inbox without consuming it. Both ordinary and stopped-reader consuming
paths observe pending signals before reading; a stopped executor/reader or host
lock cannot stop the OS watcher. Capture publication performs no fd I/O.

Expiry atomically wins against normal completion and calls **_exit(74)** without
Drop. It performs no DB/owner/generation/scratch/TX mutation, logger, formatting,
allocation, lock, join or child-ledger kill. Leases are left to expire; the next
cold boot keeps the existing recovery/quarantine rules. **74 does not certify
cleanup completion, owner release, or restart success.** A fixed diagnostic is
best effort: one nonblocking write to an independently opened pipe, or one
nonblocking SIGPIPE-safe socket send. No expiry write is attempted for regular
files, terminals or closed stderr; the original stderr flags are never changed.

**A does not satisfy D-7 and does not complete D-3a2.** Direct renderer/MCP/LSP/
shell children cannot all be assumed to have a generic lifeline. In particular,
bubblewrap's --die-with-parent is not a blanket guarantee for direct spawns or
PTY paths. Owned-child/group termination is a separately reviewed B follow-up;
the complete D-3a2 guarantee requires both A and B. The expiry tail deliberately
does not call lock-taking ProcessGroup/ledger cleanup.

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

Per-incarnation ToolSnapshot is the boundary authority, separate from the
standard-eight preflight identity. M2 snapshots the actual sent ProviderInput
before Commit against the full build-owned base-executor registry plus runtime
META_OPS and host builtins. The role-independent name set comes from
AgentRuntime::restart_builtin_tool_names(); callers constructing RestartMetadata
must use that accessor. Metadata never filters the actual build registry, and
storage requires exact name-set equality. Visible entries overlay the exact
effective specs (including run_js hints and provider edit narrowing); hidden
entries keep their raw registry specs, with registry order preserved. Planned
recovery compares the current full builtin
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

Normal model construction and recovery share ModelCatalogResolver: the startup
project uses the XDG models.dev cache, other registered projects use
config_dir/catalog (including after reload). Preload deduplicates only the
current projects' effective directories, and publication/refresh stays with each
canonical project's catalog. A missing default-project cache cannot borrow
another project's metadata to admit a graph. UsagePricing remains unchanged.

Public restart/quarantine operations and capability publication, client
reconnection and CLI continuity remain 5a/5b work. D-3a2-A adds only the core
watchdog described above; owned-child coverage remains the separate B follow-up.

### Product and full-core contract matrix

| Contract | Executable coverage |
| --- | --- |
| Startup default project recovers with an XDG-only catalog, preserving provider wire/cache prefix | `startup_recovery::startup_default_project_recovers_from_xdg_cache_only` |
| Non-startup project retains config_dir/catalog and its canonical pins; no cross-project catalog grant | Host `bootstrap::recovery::catalog::tests::nonstartup_project_keeps_its_config_catalog_and_canonical_pins_without_admission` |
| Different-build raw exec with unchanged catalogs preserves input-derived cache prefix | `planned_exec_cache::raw_exec_different_build_unchanged_catalog_preserves_cache_prefix` |
| Changed builtin catalog grants exactly one tools-only boundary, then reuses the new prefix | `planned_exec_cache::raw_exec_different_build_changed_catalog_has_one_tools_only_boundary` |
| The successor's endpoint record, hello and launcher-expected epoch are the transaction's `successor_epoch` (strictly above the predecessor's); the projection starts at it, so the predecessor's subscription cursor gets `ResyncRequired(EpochChanged)` | `planned_exec::prepared_raw_exec_reaches_product_bootstrap_with_multiple_checkpoints` (`assert_successor_epochs`), host `projection::tests::a_fixed_epoch_is_the_hello_epoch_and_the_cursor_epoch_and_rejects_the_previous_one` |
| Changed builtin catalog preserves disconnected MCP specs and prefix across raw exec | `planned_exec_cache::raw_exec_changed_builtin_catalog_preserves_disconnected_mcp_prefix` |
| Actual prepared raw exec, same PID/image, uninterrupted lifetime exclusion | `planned_exec::prepared_raw_exec_reaches_product_bootstrap_with_multiple_checkpoints` |
| Inherited FD validation before new inbox/DB; no invalid-entry Cold fallback | `planned_exec::product_rejects_unexpected_inherited_fd_without_cold_fallback` |
| One physical adoption, Ready, rejected second entry without row mutation | Positive product process scenario (independent DB and /proc readback) |
| Three same-owner parked checkpoints and capture digest survive exec | Positive product process scenario (member/snapshot/owner inventory readback) |
| Planned owner proof claims parked-only/non-resumed graph owners; ordinary shutdown releases them without activation or binding | Positive product process scenario (owner generation/readback, no Activated claim, command-wire missing-root rejection, shutdown Released state) |
| Mismatched owner proof never falls back to normal claim or clears a hold | `planned_exec::product_owner_proof_mismatch_leaves_the_hold_and_checkpoint_intact`: mutation only between raw exec and product entry, physical adoption then fail-closed boot diagnostic, original owner/generation retained |
| Successful product preflight but mismatched durable successor identity | `planned_exec::product_preflight_metadata_mismatch_invalidates_the_same_transaction`; same TX/capture invalidation, closed producer gate, rejected cancellation |
| Final signal restore, control/web request and delivery drain | `final_signal_restore_and_web_drain_matrix`: all 10 cells / 6 real signal deliveries; drain-only Release cells force real execve ENOENT after owner stop |
| Stages 9–11 failure/Stop/shutdown and raw synchronous latch ordering | Three stage matrices plus `later::latch` tests |
| Current command bindings reflect successful resumed roots, including owner retry | `startup_recovery` ordinary CommandHost follow-ups; `startup_owner_blocked_retry_binds_the_captured_root_before_ordinary_send` |
| Activation/publication gap cannot create a new conversation | `workbench-host::recovery_bindings`: protocol Busy/SameRequest, unchanged submission retry, no new run; error/unwind/cancel unfence; newer-root non-overwrite |
| Pre-ACK tools match provider-visible edit selection | `runtime::compose::tests::edit_tools::planned_preview_matches_provider_wire_for_edit_patch_switchable_readonly_and_child`: 10 HTTP cells, ordered names/descriptions/schemas; unresolved selection negative |
| Same-build tools drift never grants a schema boundary | `startup_same_build_tools_difference_is_needs_attention_without_provider_admission` |
| Missing predecessor tool evidence does not grant activation/binding | `startup_missing_predecessor_tool_evidence_never_activates_or_publishes_bindings` |
| Cold retains quarantine, never promotes Captured into runnable binding | `startup_cold_captured_graph_never_adopts_or_calls_the_provider` and cold/ACK-loss subprocess checks |

Recovery assembles current-directory bindings outside CommandHost's mutex.
Only roots admitted by CurrentRecoveryAuthority and selected for resume are
candidates. A per-thread RAII admission fence spans activation outside the host
lock and the short successful publication; a command arriving in that gap gets
Busy with SameRequest advice (the existing wire vocabulary), never a new run.
Error, unwind and task cancellation remove the fence without publishing a
binding. Initial startup does not serve Ready before this work; the same fence
also protects retries while an already-ready core serves commands. Neither the
host mutex nor its fence mutex is held through await, runtime activation or DB
work. This is routing synchronization, not a grant of execution authority.
Cold still quarantines Captured graphs; this change does not widen cold recovery.

Planned bootstrap claims every same-TX owner hold with the inherited physical
proof, including parked-only threads and graphs rejected by current authority.
Ownership exclusion is not execution authority: those checkpoints and quarantine/
NeedsAttention remain intact, with no implicit activation or command binding.
Mismatched proof leaves the hold untouched and records a diagnostic; ordinary
claim is never a fallback. Claimed owners enter the successor inventory and its
normal shutdown quiesce/release path. 5a must provide user-facing release/resume
operations for the successor-owned graphs which cannot resume automatically.

Logical graph-recovery fixtures now retain CaptureTools from the saved
ResumeIntent ProviderInput before Commit, using the production pure snapshot and
permission-fingerprint functions. They are not physical-exec evidence and never
substitute for the product process tests. The pre-artifact fixture explicitly
declares different old/new build IDs and its old compiled builtin names; the
default same-build identity remains unchanged and rejects unexplained tool drift.
This post-Ready seam records Ready before planned graph startup, just as product
boot does. An instance-scoped, once-only raw catalog override exists only under
cfg(any(test, feature="recovery-test-support")) to model a predecessor build;
normal M2 capture and visible overlay still run, and an effective compiled tool
missing from the override fails closed. No-feature product builds contain no
override field, API, or branch.

The pre-ACK preview uses the provider boundary's pure edit selection/narrowing,
not the wider authorized catalog. Routed Edit/ApplyPatch selections and a
Switchable fixed-model fallback therefore agree with the tools actually sent.
An unresolved narrowing model fails closed before FULL activation (NeedsAttention;
internal reason: `planned tool preview: current edit-tool selection unavailable`).
Production routed profiles select an interface and Switchable legacy models
explicitly select Edit; no selection is guessed from captured input.

### Remaining follow-ups

- **5a:** publish restart/capability/quarantine operations and client reconnect;
  retain these internal entry, lifetime, activation and fail-closed invariants.
- **5b:** CLI invocation/token continuity and its distinct reconnect deadlines;
  ordinary conversation follow-up routing here is not CLI invocation resumption.
- **D-3a2-B:** owned-child/group termination. A alone does not meet D-7 or
  complete D-3a2; completion requires both separately reviewed changes.
- **F4:** same-build product-to-product real exec remains separate. The raw
  cache fixtures use different predecessor/successor images.
- **MCP evidence:** no-I/O is structural with no server configured; the MCP
  seam is used only by the predecessor.
- **Affinity evidence:** ChatCompletions checks the DB and ProviderInput;
  Responses E2E separately checks wire affinity.
- **Performance:** preflight/description/rechecks hash the whole ELF roughly a
  dozen times. Private stripped process fixtures preserve executable/data
  section bytes and hash/describe/exec the same copy. Release ELF size, hash
  timing and fleet-tail costs remain **unmeasured**; no probe/boot deadline was
  extended. Safe hash reuse is a separate optimization, not assumed evidence.
