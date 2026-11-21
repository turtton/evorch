# D-3a2 B2a / B2b handoff

B2a added the typed API; B2b migrates its callers in parts. **Migration is
incomplete and D-7 is not complete.** Existing raw GroupChild/StdioSession/
ProcessGroup paths remain for the callers of the later parts and still have their
old guarantees; entry binding does **not** guard them.

## B2b-i (this part): sandbox, session, LSP, js-worker

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

Left raw on purpose until B2b-iii removes them: `StdioSession::spawn*` with a
sandbox (only tests use them now), `StdioSession::spawn_command_*`,
`WrappedCommand::to_command` and `GroupChild`.

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
| Shell pipe | tools/src/tools/shell/jobs/process.rs::run_pipe (run_pty is B3) |
| SandboxSession, LSP, js-worker | **B2b-i: migrated** (sandbox/src/session.rs raw spawn stays for tests until B2b-iii) |
| MCP | tools/src/mcp/transport/stdio.rs, ChildLease supervision |
| Eval | eval/src/evaluator/groups.rs and eval/src/verify.rs; remove creator-thread PDEATHSIG(TERM), retain normal Stop TERM |
| PR source | runtime/src/pull_requests/source.rs, **both** Some(ledger) and None branches |
| Other direct commands | tools/src/tools/{grep,git_diff}.rs; providers/src/provider/codex/quota/rpc.rs; runtime/src/{git_read,snapshot}.rs; eval/src/git.rs |
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
The actual GuardRuntime accept-loop mutation (S3), per-caller closed-stdio choices,
full caller matrix, and cancellation audit remain B2b work. Low-level peer helper
unit tests alone are not proof of the real accept loop's rejection branch.
core_instance::running_elf_identity hashes the running ELF separately; B2a does
not introduce a shared cross-crate identity authority merely to avoid this read.
