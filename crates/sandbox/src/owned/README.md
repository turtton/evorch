# D-3a2 owned process handoff (through B3-03 / B3-04)

B2a added the typed API; B2b migrated its non-PTY callers in three parts
(B2b-i, B2b-ii, B2b-iii). **D-7 is not complete**: shell PTY (B3-03) and agent Chromium (B3-04)
are now migrated. Linux raw API removal and combined evidence remain below.
Historical B2 raw-boundary details are under
"Raw boundary left for B3", with their old guarantees; entry binding does **not**
guard them.

## B2b-i: sandbox, session, LSP, js-worker

Migrated to OwnedSession/OwnedCommand/OwnedChild (guard-owned on Linux, with or
without a ledger):

- `OwnedSession::spawn_sandboxed(ledger, sandbox, spec)` (piped stdio,
  `ChildKind::SandboxSession`, never a direct-exec fallback) and
  `OwnedSession::spawn_owned(ledger, kind, label, command)` are the typed helpers.
- LSP (`tools/src/lsp/client.rs`), js-worker owner (`run_js/worker.rs`), the
  `MinimalSandbox` library listing (`minimal.rs`), the bwrap probe (`bwrap.rs`)
  and the comment-checker (`post_edit.rs`, started with
  `OwnedChild::spawn_blocking`, B2b-ii).
- Closed stdio choices: the listing and the probe pass `/dev/null` as stdin; every
  other migrated caller pipes all three streams, so none depends on an inherited
  closed descriptor.
- A caller that has no bound bundle (a test binary, an example) must be given one
  with `scripts/guard-cargo.sh`; a test that clears its child's environment has to
  pass `EVORCH_PROCESS_GUARD_BUNDLE` through.

## B2b-ii: tools, providers, runtime and eval direct commands

Migrated to the guard-owned API (guard-owned on Linux, with or without a ledger):

- Shell pipe and **PTY** jobs (`run_pipe` / `run_pty`, `ChildKind::ShellJob`;
  PTY migrated in B3-03). Both job stop capabilities are typed `OwnedGroup`s.
  Cancellation signals them outside the job's state lock.
- MCP stdio (`ChildKind::McpStdio`; `ChildLease` keeps its Drop fallback on an
  `OwnedChild`), grep (`rg`), git diff, the codex quota RPC, runtime
  `GitReader` and snapshot git, and eval's host git (no ledger: unmanaged
  `OwnedSession`).
- `OwnedChild::spawn_blocking(ledger, kind, label, command)` is the async entry:
  admission, launch and ledger registration run together on a Tokio blocking
  thread, so the synchronous guard handshake never holds an async worker, and no
  deadline is added or extended. The same applies to post_edit (S-a) and to MCP,
  whose whole `spawn_session` (admission, launch and the registration of its
  workers) runs on a blocking thread. A caller that drops the future mid-launch does
  not cancel it: the child is dropped, which kills and reaps its group, and an MCP
  session nobody took over is stopped.
- `OwnedCommand` keeps std's NUL failure: std replaces an argument, name or path
  holding a NUL with a placeholder and only fails at spawn, so the command would
  have run something else. Linux launch (and the non-Linux adapter) now return
  `InvalidInput` for it.
- Closed stdio choices: grep, git diff, git read, snapshot and eval git give the
  child `/dev/null` or pipes explicitly; none relies on an inherited closed
  descriptor. The quota RPC pipes stdin/stdout and sends stderr to `/dev/null`.
- A test that clears its child's environment and then reaches a migrated path has
  to pass `EVORCH_PROCESS_GUARD_BUNDLE` through (`tools/tests/host_environment.rs`).

## B2b-iii (this part): eval, PR source, renderer, GUI slash, raw API removal

Migrated (guard-owned on Linux, with or without a ledger):

- Eval groups (`ChildKind::Eval`) and verify (no ledger, `ChildKind::SandboxSession`,
  `OwnedChild::spawn_blocking`; the root-exit / output-drain / fixed 1 s reap phases
  are unchanged). The creator-thread `PDEATHSIG(TERM)` of the evaluation child is
  gone: the guard kills the group when the core dies, and the ledger's cooperative
  stop still sends TERM (`OwnedGroup::terminate`). `eval_start` runs the whole
  launch (`groups::launch`) on a blocking thread.
- The PR source runner, both its ledger and its ledger-less branch
  (`ChildKind::PullRequestSource`, `spawn_blocking`).
- The offline renderer (`ChildKind::Renderer`). `Generation::spawn` stays one
  synchronous admission transaction, so the guard handshake (bounded) holds the
  renderer owner's task while it runs; the helper's stdin/stdout are the IPC channel
  endpoints and stderr is `/dev/null`.
- GUI slash commands (`slash_process::run` takes an `OwnedCommand`; no ledger).
- Test and example seams: `ledger_latency`, the ledger tests, the launch-policy tests
  (now exercising the guard's policy through the owners), evorch-core's core-owner
  fixture. That fixture also tracks the owner's own guard helper: when the owner dies
  the helper is orphaned to the test's subreaper and is reaped through the owner's
  group.

Removed: `GroupChild`, `StdioSession::spawn*`/`spawn_command_*`,
`WrappedCommand::to_command`, `apply_launch_policy` and `detach_from_terminal` on
Linux. The non-Linux adapter keeps them as private (`pub(crate)`) items of its own
backend; it makes no new D-7 claim and its code is only compile-checked by CI.

### Raw boundary left for B3

Nothing a non-PTY caller can reach. What stays, and who uses it:

- `process::ProcessGroup`, `ProcessGroupGuard`, `wait_for_exit`, `has_exited` and
  `Admission::register_pty_group` (ledger `ChildGroup::Raw`): legacy non-Linux
  adapter and synthetic restart seams only after B3-03, not the Linux shell
  product caller. B3-05 removes the Linux public raw surface.
  A live `ProcessGroup` exists only from `ProcessGroup::spawn_pty`, which
  starts a command on a `portable_pty::SlavePty`; there is no constructor from a
  bare pid and no way to hand it a std or Tokio child (the `compile_fail` doctests on
  `ProcessGroup` pin that: `new` is crate-private, and a std/Tokio child is not a slave).
  `ProcessGroup::retired()` signals nothing and exists for the restart test that
  registers a synthetic receipt. `portable_pty::Child` itself would prove nothing, as
  it is implemented for `std::process::Child`.
- `process::close_inherited_fds_on_exec`: a descriptor-hygiene hook for the std
  command an evorch-core fixture (`product_exec`) starts and supervises itself. It
  owns nothing and is no way to start a product child.
- Agent Chromium is migrated by B3-04 (see below). The private core-launcher
  preflight supervisor (`--die-with-parent`) stays unchanged.

Std spawns outside the audited inventory that this part did not touch: the GUI's
short git calls (`diff/git_cli.rs`, `evorch-gui.rs`, `composer_mention.rs`,
`runtime_sink.rs`) and `fc-match` (`theme/fonts.rs`), and `core-launcher` starting the
core. They never had the launch policy. The GUI embedded-terminal PTY
(`gui/src/pty/mod.rs`, approved inventory L21) remains a separate follow-up,
not a core-owned guarantee.

### Closed stdio, per path

Every owned launch sets the three streams explicitly, so nothing depends on an
inherited descriptor. An inherited **closed** descriptor would be normalized to
`/dev/null` by the helper (`process-guard` `closed_stdio_is_sanitized_to_dev_null_by_helper_startup`);
no caller relies on that.

| Path | stdin | stdout | stderr |
|---|---|---|---|
| `spawn_sandboxed` (LSP, js-worker, sessions) | piped | piped | piped |
| MCP stdio, comment-checker | piped | piped | piped |
| minimal listing, bwrap probe | `/dev/null` | piped | piped |
| shell pipe job | `/dev/null` (synchronous) or piped | piped | piped |
| grep (`rg`) | `/dev/null` | piped | `/dev/null` |
| git diff, git read, eval git | `/dev/null` | piped | piped |
| snapshot git | piped | piped | piped |
| codex quota RPC | piped | piped | `/dev/null` |
| PR source | `/dev/null` | piped | piped |
| eval verify | `/dev/null` | piped | piped |
| eval group member | `/dev/null` | file | file |
| GUI slash command | `/dev/null` | piped | piped |
| renderer helper | IPC endpoint | IPC endpoint | `/dev/null` |

### Death matrix and cleanup evidence

`sandbox/tests/owner_death.rs` kills (SIGKILL) a separate owner process that holds
thirteen roots, each with a same-group grandchild. Every root and grandchild ends
(awaited on pidfds, no sleeps), and an unrelated process in its own group survives.
Each production entry point of the two owners is one root:

| Root (label) | Entry | Production callers |
|---|---|---|
| `mcp-stdio`, `sandbox-session`, `pull-request-source`, `shell-job`, `eval`, `probe`, `renderer`, `other` (the eight `ChildKind`s) | `OwnedChild::spawn_blocking(Some(ledger), kind, …)` | MCP, PR source and shell pipe jobs use their own kind; the rest cover the remaining kinds |
| `blocking-no-ledger` | `OwnedChild::spawn_blocking(None, …)` | eval verify, GUI slash, the PR source without a ledger |
| `admission-spawn-owned` | `Admission::spawn_owned` | eval groups, the renderer, `ledger_latency` |
| `session-spawn-owned` | `OwnedSession::spawn_owned(Some(ledger), …)` | ledger-registered sync sessions |
| `session-spawn-sandboxed` | `OwnedSession::spawn_sandboxed(Some(ledger), …)` | LSP, js-worker, sandbox sessions |
| `session-no-ledger` | `OwnedSession::spawn` | eval's host git |

It covers the typed entries every caller goes through, not the callers themselves.
**The SIGKILL matrix with the real release core running its real callers, and the
real core's expiry, are B3.** Real-caller coverage that exists today: the renderer
owner's abrupt core exit (`renderer_owner`), MCP and ledger stop (`mcp_stdio`),
eval groups (`groups` unit tests).

Mid-launch abandonment (#744 S1): `mcp_stdio::a_launch_abandoned_mid_way_...` parks
the launch on its blocking thread, drops the caller, releases it, and shows the
server and its descendant end, the ledger confirm the reap and the permit release.
The eval start path has the same shape: its launch holds the producer permit on the
blocking thread (`evaluator::tests::start::an_abandoned_launch_keeps_its_producer_permit_until_it_returns`),
so a restart cannot see the producers drained before the launch registers its
evaluations. A guard lost *after* a KILL (#744 S2):
`owned::a_guard_lost_after_a_kill_stays_unconfirmed_...`.

Closed stdio is exercised by `sandbox/tests/closed_stdio.rs`: a launcher with
descriptors 0, 1 and 2 closed starts the representative selections (all pipes,
`/dev/null` stdin, files, an IPC-shaped pair with `/dev/null` stderr).

## New contract

- OwnedCommand is target configuration, not a raw Command wrapper/conversion.
  Linux owners never accept raw Commands, arbitrary pre_exec hooks or bare PGIDs.
- Linux always requires a verified guard, with or without ledger registration.
  Non-Linux adapters keep the existing backend, without a new D-7 claim.
- Raw target status/errno and direct stdio handles are retained. Helper status 0
  is not target success. GuardLost stays sticky and ledger cleanup unconfirmed.
- exited observes RootExited/WNOWAIT only. wait is sticky even after cancellation;
  root exit then kills the reserved group and reaps root, not descendant drain.
  Async decoder state lives in the owner. Synchronous wait releases the decoder
  mutex before blocking so another thread can Stop the target.
- Drop half-closes control; async Drop transfers the exact guard and optional
  ledger entry to a reaper thread. Only FinalStatus confirms the ledger entry.
- TERM is normal Stop; force/Drop/core death use KILL. No bare-PGID fallback.
  Control requests return io::Result, not cleanup success. Transport/decoder loss
  stays sticky until wait reports failure; a reaper cannot confirm that ledger entry.
- A TERM after a KILL (control already half-closed, e.g. a supervisor's grace expiry
  after a ledger stop) is a no-op, not a lost transport; the flag is read and set
  under the owner lock together with the control write.
- Inherited **closed** stdio is normalized to /dev/null (EOF / accepted writes).
  Explicit pipes/null/files keep their semantics. Preserving EBADF is not promised;
  each caller's explicit selection is the table under "Closed stdio, per path".
- Applications bind the installed sibling bundle (not the inherited env), before
  logging/config/runtime. Core planned entry first physically adopts handoff FDs.
  Core --describe-handoff/--print-renderer-helper require binding; help/version
  remain usable without it. js-worker is a target, not an owning entry, and skips
  binding before logging. A rebuilt development app must be rebound using
  scripts/guard-artifact.sh bind-callers (or scripts/guard-cargo.sh).

## B2b non-PTY caller inventory (audited at base 794fa1ed)

| Owner | Production paths to migrate |
|---|---|
| Common sandbox/session | **B2b-i: minimal.rs, bwrap probe migrated**; **B2b-iii: raw session seams and to_command removed** |
| Shell pipe / PTY | **B2b-ii / B3-03: migrated** tools/src/tools/shell/jobs/process.rs::{run_pipe,run_pty} |
| SandboxSession, LSP, js-worker | **B2b-i: migrated** (raw session spawn removed in B2b-iii) |
| MCP | **B2b-ii: migrated** tools/src/mcp/transport/stdio.rs, ChildLease supervision |
| Eval | **B2b-iii: migrated** eval/src/evaluator/groups.rs and eval/src/verify.rs; creator-thread PDEATHSIG(TERM) removed, normal Stop stays TERM |
| PR source | **B2b-iii: migrated** runtime/src/pull_requests/source.rs, both Some(ledger) and None branches |
| Other direct commands | **B2b-ii: migrated** tools/src/tools/{grep,git_diff}.rs; providers/src/provider/codex/quota/rpc.rs; runtime/src/{git_read,snapshot}.rs; eval/src/git.rs |
| Offline renderer/CEF root | **B2b-iii: migrated** workbench-host/src/areas/browser/renderer.rs and renderer ownership test support; actual CEF E2E remains B3 |
| GUI slash | **B2b-iii: migrated** gui/src/model/slash_process.rs and the builders in external_slash |
| Examples/test seams | **B2b-iii: migrated** sandbox/examples/ledger_latency.rs, sandbox tests/ledger fixtures; evorch-core renderer_support launch (the owner/restart fixtures keep the raw PTY-style group handle) |

Audit external crates separately rather than assuming the Rust API can control
an internal spawn. The private core-launcher preflight supervisor and bwrap
--die-with-parent stay unchanged. Agent Chromium is migrated in B3-04 (below); shell PTY is migrated in B3-03 (below), while GUI PTY remains excluded.

## B2b completion and optional further split

1. Migrate every inventory row, including ledger-less and error/Drop paths. Fix
   types in all affected tests; remove Linux raw-command owner entrypoints.
2. Keep the old PTY backend private to PTY; prove non-PTY cannot reach it. Reject
   raw Command at compile time, not by naming convention or a runtime flag.
3. Regress verify's root-exit / output-drain / fixed 1s reap phases; session
   try_wait/output/Drop; status/errno; cancellation followed by wait.
4. Creator-thread retirement survives for direct and bwrap children (including
   LSP). Real owning-core SIGKILL matrix covers eight kinds plus no ledger,
   root and same-group grandchild, and preserves an unrelated process.
5. Preserve js-worker boundary and its mutation; bind missing/mismatch/env cases;
   cache contracts and A's 34 cells. Do not extend deadlines or A's exit-74 tail.
6. Keep incomplete scope explicit until migration is done. Accepted B3 residuals:
   PTY/agent Chromium, loaded release-core expiry, real Chromium/CEF E2E and
   performance gates. No D-7 completion claim from B2a or B2b.

If B2b exceeds a reviewable size, split into B2b-i (sandbox/session/LSP/js-worker),
B2b-ii (tools/MCP/shell/other direct), B2b-iii (eval/runtime/renderer/GUI and final
raw API removal). Keep additive adapters until the last part; all intermediate
PRs must say migration is incomplete. Re-check file counts after B2a: the original
55–75-file estimate included API, caller, test and entry changes, not 55–75 caller
files alone. Register/serialize central edits before each part.

B1 follow-ups: saved-fd3 dash fixture and artifact-script CI are addressed in B2a.
The actual GuardRuntime accept-loop mutation (S3) is addressed in B2b-i
(`accept_helper` and its rejection tests). Per-caller closed-stdio choices are
recorded for every migrated caller ("Closed stdio, per path"); the death matrix and
the cancellation audit are in "Death matrix and cleanup evidence". Low-level peer helper
unit tests alone are not proof of the real accept loop's rejection branch.
core_instance::running_elf_identity hashes the running ELF separately; B2a does
not introduce a shared cross-crate identity authority merely to avoid this read.

## B3-04 agent Chromium: owned launch + connect

Agent Chromium now uses unmanaged/no-ledger `OwnedCommand` / `OwnedChild`
(`ChildKind::Other`), then `Browser::connect_with_config`. It no longer calls
chromiumoxide's hidden `Browser::launch`. The launch adapter preserves C-13's
argv (including duplicates), full environment, cwd and inherited stdin / null
stdout / piped stderr. All seven HandlerConfig fields are explicit; notably
`ignore_https_errors = false`, not HandlerConfig's insecure default.

Endpoint collection has one absolute 20-second post-ExecOk deadline, a total
256-KiB budget (including the endpoint) and a 16-KiB line budget. EOF, invalid
UTF-8/URL, non-loopback endpoint and either overflow fail closed. Success closes
stderr; no drain/logging task survives it. Cancellation, initialization errors,
handler-ended reset and Drop keep the child, profile and proxy in one owner.
A dedicated reaper removes the profile only after confirmed reap. GuardLost or
failure to start that reaper retains the profile with a diagnostic, never a
cleanup-success claim. A typed-spawn error without reap evidence is conservatively
unconfirmed too. Proxy accept/connection/tunnel/upstream tasks and CDP
handler/gate tasks are scoped and cancelled rather than detached.

Linux `browser/tests/agent_chromium.rs` includes a real agent navigation followed
by test-owner exit 74, test-owner SIGKILL, normal Drop and repeated relaunch after
handler termination. The external subreaper observes the actual tree and awaits
pidfds for **every observed same-PGID member**; it separately reclaims escapees
on success or failure. Exit 74 here is a fixture owner using the production
AgentBrowser path, **not the release core's real 10-second expiry** (B3-07/08).
`agent_observation_restart` is currently a registration-only placeholder; its
success is not evidence of a planned-exec Chromium connection. Planned exec's
CLOEXEC control loss kills the old browser group; reuse/reconnection is not
promised. Profile cleanup cannot run when the owner dies without destructors.

**Known remaining leak / outside the same-PGID guarantee:** Chrome
157.0.8103.78's preflight navigation produced two `chrome_crashpad_handler`
processes in separate SIDs/PGIDs (browser root 3029375 in PGID 3029066;
Crashpad 3029384 in SID/PGID 3029383, and 3029386 in SID/PGID 3029385).
The owned-launch trace likewise records Crashpad leaving the browser's new
SID/PGID while renderer/zygote/GPU/utility descendants stay in it. Abrupt owner
death therefore does **not** guarantee these Crashpad helpers terminate. Tests
record that fact separately; their outer pidfd cleanup must not mask or weaken
the same-group death assertion. Do not add disable-crash-reporter flags or a
cgroup here: either needs a separate argv/C-13 design review. Carry this into
B3-09's excluded-spawn audit/follow-up list. PTY migration, real loaded-core
expiry, real CEF and performance gates remain B3; this is **not D-7 completion**.

## B3-03: typed shell PTY ownership

- `WrappedCommand::to_owned_pty_command` yields a target-only description.
  `OwnedPtyChild::spawn_blocking(ledger?, kind, label, command, size)` performs
  admission, openpty, authenticated ExecOk and the existing typed ledger
  registration together off the async worker. No raw command/slave/PGID enters.
- The pending launch is RAII-owned before registration; an unwind retires it on
  that blocking thread before admission is released. Abandoning the await does
  not abandon the completed result: its normal owner transfers cleanup to the
  shared off-worker reaper. IO setup errors follow the same Drop path.
- Linux uses B3-02's opaque master reader/writer/resize and shared GuardChild
  status/decoder. EIO remains EOF, writer Drop sends EOT, shell PTY signal exits
  keep portable-pty's shell-facing code 1 (raw Unix status is retained by the
  owner). Job timeout starts at the same post-launch supervision point and its
  meaning is unchanged. No new product grace or deadline was added.
- `FinalStatus` / ledger confirmation mean root reap and group-KILL submission,
  not all descendants' exit/reap. GuardLost remains an error and an unconfirmed
  ledger entry. Same-PGID descendants are covered; job-control/setsid/other-PGID
  escapees are not. Tests explicitly observe and reclaim one escapee.
- Non-Linux stays on portable-pty and the old group backend behind a typed
  adapter; no guard or stronger core-death guarantee is claimed.
- Focused coverage: `tests/owned_pty.rs` (isolated subreaper, native tty /
  foreground / resize / Ctrl-C / EOT, sh-c root and descendants, drop, GuardLost,
  errno/fence/openpty and reader/writer setup errors), private cancellation /
  registration-unwind tests, shell status/cancel/real timeout/supervisor abort,
  and existing shell input / restart tests. Post-retirement descendant probes
  distinguish exit from a surviving ACK without timing assertions. No /dev/ptmx
  pathname identity or bash-only shell syntax is assumed.

B3-05 still owns removal of the public Linux raw process/ledger variants and
restart synthetic receipt seams. Agent Chromium was migrated separately in
B3-04 (#756), already in this base; real-core SIGKILL / expiry (B3-06/07),
CEF and performance evidence remain separate. These synthetic
API/caller tests do not prove real release-core matrix cells. **D-7 is incomplete.**

### B3-03 focused verification and source-only mutations

Local validation used the normal dev shell (turtton Cargo turtton.5), serial
builds and a freshly sealed development helper bundle. The final restored tree
passed 22 selected sandbox tests (20 existing direct-owner regressions plus
PTY/cancellation controllers), 6 selected shell tests, scoped all-target clippy
with warnings denied, formatting and diff checks. Other tests are delegated to CI.
The PTY controller runs 13 isolated cases; these are not 13 real-core callers.
The 6 selected shell tests also passed with dash 0.5.12 as `sh`, as well as
the normal bash environment. Non-Linux execution was not tested locally.

Each helper mutation was rebuilt and sealed with its own compat; all mutations
were restored. Each negative failed after the actual target had emitted READY:

| Temporary mutation | Observed failure |
| --- | --- |
| Signal root only rather than its group | Same-group descendant ACKed the post-retirement probe |
| Omit retirement group KILL | Same-group descendant ACKed the post-retirement probe |
| Omit abandoned-owner Drop cleanup | After READY, cancelled blocking launch retained GuardChild instead of handing ownership to cleanup |
| Omit foreground installation | Native READY reported target PGID different from foreground PGID |

Isolated controller RAII reclaims observed processes on failure, including
escaped-PGID children. No product fault flags or product grace periods were
introduced. The two former shell watcher tests used a busy loop, wall-clock
polling and fixed completion timeouts; their same-group property is now checked
by the native/sh-c controller and their shell cancel/status property by READY /
pidfd-driven supervision tests.

The initial CI run exposed a hang in the new shell timeout test: real PTY IO
was mixed with paused/advanced Tokio time. The old-head test was reproduced
under dash with capture enabled and a temporary 10-second runner bound
(TIMEOUT at 10.002 s). The correction removes the virtual clock entirely and
passes a real 200 ms job timeout through the existing API. At the operator's
explicit request this regression has a fixed, failure-only 5-second observation
bound plus typed stop/task-abort cleanup, rather than waiting forever if the
job deadline is broken. A temporary source-only missing-timeout negative failed
at that assertion in 5.021 s (not the runner timeout) and exited; it was restored.
All reproduction commands have a 300-second outer bound, use capture, and do not
add a product fault hook. A grep survey also found existing mixed real-child /
virtual model-wait tests in shell/job_tests.rs; those out-of-scope tests were
left unchanged.

Independent review follow-up: the cancellation controller captures root/helper
pidfds at READY, while the registration gate still keeps both alive, and retains
those identities for Drop (no late numeric-PID reopen). A related grep survey
found /proc-child discovery in the integration reaper; it now validates the
opened fd with P_PIDFD / WNOWAIT before signalling, and rejects the isolated
controller itself as a non-child. Existing process-guard controller fallbacks
were reported separately, not edited here. The timeout case no longer requires
READY (or a live numeric PID) before a valid 200 ms deadline: it checks typed
Pty/OwnedGroup registration, one ledger entry, its receipt/reap, zero unconfirmed
entries and retirement. A temporary PATH sh shim sleeps 400 ms before execing
dash: the previous test failed with `supervisor ended before READY: timed_out`
(1.866 s), while the corrected test passed (2.240 s). The corrected sandbox
PTY/cancellation controllers (2 selected tests), 6 normal shell regressions and
scoped sandbox/tools all-target clippy also passed. The shim is not product code.
