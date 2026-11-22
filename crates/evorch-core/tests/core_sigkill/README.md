# B3-06 caller SIGKILL matrix (Linux)

Separate harness=false integration leaf. The existing 13 API routes in
sandbox/tests/owner_death.rs remain separate, unchanged evidence; they are
**not** 13 product callers or the historical 11 roots. The approved inventory
is docs/single-core/d3a2-B3-prework.md.

## Evidence classification

P = sealed feature-free release evorch-core closure. P* = that closure plus
external dependencies/provider configuration. T = test/API owner, **not** a
product-ELF reachability claim. P/P* start the sealed ELF through existing
entrances, with no bootstrap feature or administrative hook. External
commands are EOF-resistant root/same-PGID-grandchild fixtures (shell peers
ignore HUP so PTY master close cannot mask absent ownership); core caller,
sandbox and ownership APIs are real. MinimalSandbox/bwrap is real;
DirectSandbox is only T.

| Inventory | Test | Evidence | Existing entrance / actual caller | ChildKind; ledger |
|---|---|---|---|---|
| L01 | shell | P* | Conversation CreateExecution → agent shell (pipe) | ShellJob; yes |
| L02 | shell_pty | P* | Same tool, interactive PTY | ShellJob; yes |
| L03 | grep | P* | agent grep → external rg | Other; yes |
| L04 | git_diff | P* | agent git_diff → external git diff | Other; yes |
| L05 | comment_checker | P* | agent write → post-edit checker | SandboxSession; yes |
| L06 | lsp_t | T | real LspDiagnostics explicitly registered | SandboxSession; yes |
| L07 | mcp | P* | MCP Refresh/PreviewApproval/ConfirmApproval/TestConnection | McpStdio; yes |
| L08 | js_worker | P* | agent run_js → actual evorch js-worker entry | SandboxSession; yes |
| L09 | libraries | P* | run_js preparation → library detection (bwrap --list) | SandboxSession; yes |
| L10 | bwrap_probe | P | core startup → bwrap feature probe | Other; yes |
| L11 | quota_t | T | real CodexQuotaClient app-server RPC | Other; yes |
| L12 | eval | P* | delegate → Evaluator → eval_save_sample → approved eval_start → groups::launch; external CLI is a fixture | Eval; yes |
| L12 | eval_cli | P* | same entrance; the CLI is the **real** `evorch eval run` (only the provider is the offline mock) | Eval; yes (CLI); replay tools: guard-owned, no ledger |
| L13 | renderer_t | T | real RendererOwner.open; external executable fixture | Renderer; yes |
| L14 | chromium_t | T | real AgentBrowser.page; external executable fixture | Other; **no** |
| L15 | pull_request | P* | PR query + LinkPullRequest → external gh | PullRequestSource; yes |
| L16 | git_read | P | repository/thread-start git read | Other; yes |

L12: **external CLI replaced by a fixture; core caller and API are product
code**. Recorded argv must contain eval run, --thread sample, candidate,
problem, repeat, output and user-config. A replayable sample is created via
public conversation commands and an isolated git repository, then pinned by
Evaluator. The real CLI is the separate `eval_cli` cell below.

### eval_cli: the real `evorch eval run` (P*)

Same product entrance as `eval`; the external command is the actual
`evorch` CLI, only the model provider is the offline mock. The cell installs
the CLI like a distribution: the ELF from EVORCH_B3_CLI_EXE, the
`guard-bundle.json` and helper that the build's bind step
(scripts/guard-cargo.sh, run by CI and locally) wrote beside it, and a sibling
`evorch-core` (the launcher refuses a CLI without one). `evorch` binds
itself to that bundle (role `cli`) before doing anything but `js-worker`,
`--help` and `--version`. A missing or unbound bundle fails the cell with the
reason; it is never skipped and never made up.

**Structure: not a second owner level.** `evorch eval run` is a thin client
of the core running for `--user-config` (the same config dir, so it attaches
and starts no core) and owns no process; the replay is a job inside the core,
so its tools hang under the core's guard like L01. The cell asserts exactly
that, and does **not** claim a core → CLI → CLI-owned-child chain:

1. core ← guard (argv names the core) ← real `evorch eval run`, which is the
   guard's root process itself, not a wrapper's child (argv checked:
   eval run, --thread sample, --candidate, --problem, --repeat 1, --output,
   --user-config); the CLI has no child and no `evorch-core` copy was
   started.
2. core ← guard ← bwrap ← root ← same-PGID leaf: the replay's own shell tool,
   run by the mock's first replay request. The replay's tool executor is built
   without a spawn ledger, so only guard ownership is claimed for it. The
   replay sandbox shows only its workspace, the system directories and the
   host's Cargo caches (read-only), so the peer binary and control socket sit
   in `$HOME/.cargo/bin`, mounted at `/tmp/home/.cargo/bin`.

The mock script entry that starts the peer is attributed to the replay (only
replay requests carry the `Run identifier:` preamble), so an Evaluator or
orchestrator request cannot be mistaken for it.

**Event synchronisation.** The injection waits for READY from the replay's
peer. That proves the replay was inside a tool call; no API names a running
job's state, so it is not a read of the job operation. Nothing waits on time.

**Observed:** after the core alone is SIGKILLed, pidfd death (before any
cleanup) of the CLI and of the replay peer's whole group, and for both guards
the strace order (group KILL before root reap, no group signal after).

**Not claimed:** that the CLI died *because of* its guard rather than its lost
connection (both end it; ownership is asserted from the guard ancestry, and
the replay group, which has no such natural exit, is the discriminating
part); anything about the job finishing; a detached core started by a CLI that
finds none (not guard-owned, not reclaimed, not tested here).

L13/L14: **core_owner:false; T/API evidence, not product P* evidence**.
They prove launch ownership, not renderer IPC/CDP handshake or navigation.
Product entrances and navigation remain B3-08. No real CEF/Chrome result is
claimed. ProcessTree::new(owner_pid) accepts that future product owner.

Additional T cells: shell_direct_t (real shell/DirectSandbox), js_direct_t
(actual worker entry/injected DirectSandbox), legacy_stdio_t (OwnedSession
adapter for removed StdioSession slot), probe_t (API-only Probe, no invented
product caller). Eight kinds plus legacy stdio, PTY and JS map to the
historical 11 roots; real Chrome/CEF are extra cells. LSP/quota are T, not
counted among the approved 14 product caller candidates.

Product ledger columns describe **source wiring**, not private runtime
introspection: BootstrappedHost creates one SpawnLedger and clones it into
executor/components, MCP, evaluation, PR adapters, renderer and runtime
thread-start paths (workbench-host/bootstrap/{mod,components,host_services}.rs;
runtime/{compose,thread_start,repository}.rs). Chromium intentionally has no
ledger. T owners report actual ledger children and assert expected kind.
Observer evidence is independent of ledger membership.

## Independent observations

Private Unix-socket READY follows fixture-child readiness. Peer credentials
identify the host-namespace root PID. All targets are alive/pidfd-pinned
before injection. Validated /proc records include parent, PGID, session and
starttime; guard argv must name the launched core/test owner. Only that
owner's pidfd receives SIGKILL. Target pidfds must report death **before
cleanup**. Socket EOF/ECONNRESET alone is not the death assertion.

L08 has two distinct structures: outer bwrap → inner bwrap → actual worker,
and a separate core-owned HostCall shell root + same-PGID grandchild. Worker
has no process children. Its own pidfd death, initial exec followed by no
new process (strace), and HostCall shell pair death are separate
observations. An unresolved finally Promise prevents EOF making it pass.
The copied CLI directory has no installed guard bundle: successful worker
entry also proves the bind-before-logging exclusion. CLI is an actual build
from this source; its SHA256 is recorded separately from release core.

Daemonized strace (-D -ff --decode-pids=pidns) is independent evidence and never kills tracees. -ff writes one log per TID, so the no-spawn check follows every thread of the worker (thread ids are mapped from the tracee namespace to the host by --decode-pids=pidns, followed through nested CLONE_THREAD clones, exited threads included): thread-creating clones are allowed, while fork, vfork and process-creating clone/clone3 are violations in any thread. An execve after the initial exec is also rejected in the leader and, best effort, in other threads. Detecting a *successful* exec in a non-leader thread is a follow-up: strace splits it into `execve(... <pid changed to ...>` and `<... execve resumed>) = 0`, which this check does not join. The acceptance criterion is that no new process is created. The negative control observer_trace_thread runs a real strace of a process whose only thread forks a short-lived child and requires rejection; a thread-only process is accepted.
It records group SIGKILL before root wait4 and rejects later group signals.
CI installs strace/bubblewrap, checks ptrace and /dev/ptmx, enables Ubuntu
user namespaces and explicitly builds the actual CLI. Missing support fails,
never silently skips. EVORCH_B3_EVIDENCE_DIR retains JSON READY, death, argv
and syscall evidence; CI uploads it per runtime shard.

## Bounds and execution

Parent-fixed failure watchdogs: READY 30s, death 30s; whole-cell 120s followed
by 30s controller cleanup. Success comes from explicit events/pidfds, never
elapsed time. No virtual Tokio time with real process I/O, no load or speed
acceptance threshold. Cleanup follows success/failure/panic, uses retained
pidfds, discovers adopted descendants and reaps to ECHILD in a dedicated
subreaper. Root-only/escaped-PGID controls prove cleanup cannot hide a leak;
stale-ancestor injection exercises post-pin graph recomputation.

Build/seal via scripts/product-artifact.sh and export its environment. Set
EVORCH_B3_CLI_EXE to an actual evorch binary and EVORCH_B3_BWRAP_EXE to real
bwrap if absent from PATH. Run cargo nextest run -p evorch-core --test
core_sigkill --test-threads 1 under timeout -k, with normal capture. Local
acceptance: three consecutive passes; CI one run/cell.

Actual ten-second expiry/74 is B3-07, not here. Preflight regressions are
supplied by a **separate PR**. B3-05 raw API, FD hygiene, m2 and process-guard
test paths are untouched.

Mutations run only in a disposable checkout; code/runners are not committed.
PR results distinguish all-caller guard bypass (owner-pidfd AND EOF forced
kill disabled), representative root-only kill, early reap and raw spawn.
Removing pidfd alone may PASS because EOF is redundant. Raw-spawn detection
combines the representative mutation and B3-05's typed compile-fail boundary.
Optional control-FD-keeper pidfd-isolation proof remains follow-up if not run.

Normal/CI PTY cells execute real bwrap **directly**, without a bwrap wrapper.
Only the disposable guard-bypass mutation wraps real bwrap with an ignored
HUP disposition. Otherwise PTY master close kills the bwrap session leader
and its parent-death chain can hide the absent guard, even when the peer
ignores HUP. This mutation-only isolation is not a product-ELF claim.
