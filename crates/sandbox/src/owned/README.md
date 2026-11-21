# D-3a2 B2a / B2b handoff

B2a added the typed API; B2b migrated its non-PTY callers in three parts
(B2b-i, B2b-ii, B2b-iii). **D-7 is not complete**: the pseudo-terminal shell jobs
and agent Chromium (B3) still run on the raw backend described under
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

- Shell **pipe** jobs (`run_pipe`, `ChildKind::ShellJob`). The job's stop
  capability is a `JobGroup`: the guard-owned `OwnedGroup` of a pipe job or the
  raw group of a PTY job (`run_pty` is B3). Cancellation signals it outside the
  job's state lock.
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
  `Admission::register_group` (ledger `ChildGroup::Raw`): the shell **PTY** jobs
  (`run_pty`, started by `portable-pty`) and the restart test that registers a raw
  group. They take a numeric group handle and a raw child; they start nothing.
- `process::close_inherited_fds_on_exec`: a descriptor-hygiene hook for the std
  command an evorch-core fixture (`product_exec`) starts and supervises itself. It
  owns nothing and is no way to start a product child.
- Agent Chromium (`crates/browser`, chromiumoxide's own spawn) and the private
  core-launcher preflight supervisor (`--die-with-parent`): unchanged.

Std spawns outside the audited inventory that this part did not touch: the GUI's
short git calls (`diff/git_cli.rs`, `evorch-gui.rs`, `composer_mention.rs`,
`runtime_sink.rs`) and `fc-match` (`theme/fonts.rs`), and `core-launcher` starting the
core. They never had the launch policy.

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
eleven roots, each with a same-group grandchild: the eight `ChildKind`s through
`OwnedChild::spawn_blocking` with a ledger, a ledger `SandboxSession`
`OwnedSession`, and a child and a session with no ledger. All roots and grandchildren
end (awaited on pidfds, no sleeps), and an unrelated process in its own group
survives. It covers the typed owners every caller goes through, not the real release
core running its callers (B3's "real core expiry"). Real-caller coverage that exists
today: the renderer owner's abrupt core exit (`renderer_owner`), MCP and ledger stop
(`mcp_stdio`), eval groups (`groups` unit tests).

Mid-launch abandonment (#744 S1): `mcp_stdio::a_launch_abandoned_mid_way_...` parks
the launch on its blocking thread, drops the caller, releases it, and shows the
server and its descendant end, the ledger confirm the reap and the permit release.
A guard lost *after* a KILL (#744 S2): `owned::a_guard_lost_after_a_kill_stays_unconfirmed_...`.

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
| Shell pipe | **B2b-ii: migrated** tools/src/tools/shell/jobs/process.rs::run_pipe (run_pty is B3) |
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
--die-with-parent stay unchanged. Agent Chromium and all PTY owners belong to B3.

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
recorded for the callers migrated so far; the remaining callers' choices, the full
caller matrix and the cancellation audit stay B2b-iii work. Low-level peer helper
unit tests alone are not proof of the real accept loop's rejection branch.
core_instance::running_elf_identity hashes the running ELF separately; B2a does
not introduce a shared cross-crate identity authority merely to avoid this read.
