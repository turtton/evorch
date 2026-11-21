# D-3a2 B2a / B2b handoff

B2a added the typed API; B2b migrates its callers in parts. **Migration is
incomplete and D-7 is not complete.** Existing raw GroupChild/StdioSession/
ProcessGroup paths remain for the callers of the later parts and still have their
old guarantees; entry binding does **not** guard them.

## B2b-i: sandbox, session, LSP, js-worker

Migrated to OwnedSession/OwnedCommand/OwnedChild (guard-owned on Linux, with or
without a ledger):

- `OwnedSession::spawn_sandboxed(ledger, sandbox, spec)` (piped stdio,
  `ChildKind::SandboxSession`, never a direct-exec fallback) and
  `OwnedSession::spawn_owned(ledger, kind, label, command)` are the typed helpers.
- LSP (`tools/src/lsp/client.rs`), js-worker owner (`run_js/worker.rs`), the
  `MinimalSandbox` library listing (`minimal.rs`), the bwrap probe (`bwrap.rs`)
  and the comment-checker (`post_edit.rs`, `OwnedChild` through
  `Admission::spawn_owned`).
- Closed stdio choices: the listing and the probe pass `/dev/null` as stdin; every
  other migrated caller pipes all three streams, so none depends on an inherited
  closed descriptor.
- A caller that has no bound bundle (a test binary, an example) must be given one
  with `scripts/guard-cargo.sh`; a test that clears its child's environment has to
  pass `EVORCH_PROCESS_GUARD_BUNDLE` through.

## B2b-ii (this part): tools, providers, runtime and eval direct commands

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

Left raw on purpose until B2b-iii removes them: `StdioSession::spawn*` with a
sandbox (only tests use them now), `StdioSession::spawn_command_*` and
`GroupChild` (non-Linux adapter, eval groups/verify, PR source, renderer, GUI
slash), and `WrappedCommand::to_command` (eval verify).

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
  Explicit pipes/null/files keep their semantics. B2b must document each caller's
  selection; preserving EBADF is not promised by this new API. No caller changes
  in B2a imply a change to its current stdio behavior.
- Applications bind the installed sibling bundle (not the inherited env), before
  logging/config/runtime. Core planned entry first physically adopts handoff FDs.
  Core --describe-handoff/--print-renderer-helper require binding; help/version
  remain usable without it. js-worker is a target, not an owning entry, and skips
  binding before logging. A rebuilt development app must be rebound using
  scripts/guard-artifact.sh bind-callers (or scripts/guard-cargo.sh).

## B2b non-PTY caller inventory (audited at base 794fa1ed)

| Owner | Production paths to migrate |
|---|---|
| Common sandbox/session | **B2b-i: minimal.rs, bwrap probe migrated**; to_command/raw session seams stay until B2b-iii |
| Shell pipe | **B2b-ii: migrated** tools/src/tools/shell/jobs/process.rs::run_pipe (run_pty is B3) |
| SandboxSession, LSP, js-worker | **B2b-i: migrated** (sandbox/src/session.rs raw spawn stays for tests until B2b-iii) |
| MCP | **B2b-ii: migrated** tools/src/mcp/transport/stdio.rs, ChildLease supervision |
| Eval | eval/src/evaluator/groups.rs and eval/src/verify.rs; remove creator-thread PDEATHSIG(TERM), retain normal Stop TERM |
| PR source | runtime/src/pull_requests/source.rs, **both** Some(ledger) and None branches |
| Other direct commands | **B2b-ii: migrated** tools/src/tools/{grep,git_diff}.rs; providers/src/provider/codex/quota/rpc.rs; runtime/src/{git_read,snapshot}.rs; eval/src/git.rs |
| Offline renderer/CEF root | workbench-host/src/areas/browser/renderer.rs and renderer ownership test support; actual CEF E2E remains B3 |
| GUI slash | gui/src/model/slash_process.rs and caller command builders in external_slash/external_commands |
| Examples/test seams | sandbox/examples/ledger_latency.rs, sandbox tests/ledger fixtures; evorch-core renderer_support launch and owner/restart fixtures |

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
