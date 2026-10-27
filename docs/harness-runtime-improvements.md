# Harness reliability improvements

Implementation branch: `codex/harness-reliability`. The operator approved the
limited restore expansion and finalization correction on 2026-09-23. The accepted
contract is [ADR 0027](../intents/evorch/decisions/0027-restore-contract.md).

Working memory now uses a latest working note and explicit original-evidence
access at context and escalation boundaries. See [Working notes](working-notes.md)
for its persistence, approval-provenance, cache, and evaluation contracts.

## Execution and interaction

- Runtime meta tools expose concrete JSON schemas. Schema acceptance and argument
  deserialization are checked together, including roles and structured failures.
- Every `shell` start is supervised as an owner-scoped job from launch. With
  omitted `yield_ms`, short commands retain the `exit_code` result format.
  Model tool calls automatically return a continuing job after **10,000 ms** or
  accepted host input, without restarting or reapproving the command. This
  releases the shell wait, not necessarily the next model request: other tools
  in the batch can still be waiting. Direct callers and the benchmark verifier
  use `ShellWaitPolicy::UntilTerminal` and never judge a running job as success.
  Omitted-yield starts close stdin (pipe null input / PTY EOF). Model starts
  (`ShellWaitPolicy::AutoYield`) and explicit-yield jobs default to a one-hour
  lifetime, so a job that a poll waits on cannot run unattended forever; direct
  `UntilTerminal` callers keep no default timeout. Use an explicit `yield_ms` to
  retain stdin. `timeout_ms` applies to the original process lifetime and has no
  upper limit.
- Explicit start `yield_ms: 0..1800000` waits for completion or its deadline,
  ignoring intermediate output, then returns a `job_id` and cursor. `poll`
  **ignores `yield_ms` and has no deadline**: it waits until the job finishes or
  new input arrives for the run (host input, or a message from its parent), and
  never returns for new output alone. One poll therefore covers a long, noisy
  build in one provider turn; the earlier sixty-second default cost a turn per
  minute, and models often polled every second. Input ends the poll without
  stopping the job, and the result says so. For an interactive prompt with no
  input to send, explicitly use `return_on: "output"` on poll; that poll also
  returns once retained output exists beyond its cursor. `stdin` preserves its
  output-or-completion wait and defaults to zero; `stop` also defaults to zero
  and waits for teardown/deadline. Both have the existing 60,000 ms maximum.
  Zero returns immediately. Output never extends a deadline.
- Time a poll spends blocked (`ToolResult::idle_wait`, runtime-only) is excluded
  from `max_elapsed`, like the Waiting phase of `wait`, so a long build no longer
  exhausts a Worker's elapsed budget while it only waits.
  Use `{"action":"stop","job_id":"...","yield_ms":1000}` to stop the process
  group and wait up to one second for teardown, then poll to terminal if needed.
  Stop does not roll back prior filesystem effects.
- Output cursors remain redacted UTF-8 byte offsets. An omitted cursor continues
  after the highest cursor returned. When input ends a completion-oriented
  poll while the job still runs, its omitted-cursor preview favors the last
  8 KiB / 200 lines of new retained output; skipped bytes are flagged with
  `output_gap`. Explicit cursors (including zero) keep ordered paging and allow
  re-reading retained skipped output. Completion artifacts preserve bounded
  stdout-then-stderr output for omitted-yield starts (explicit jobs retain
  interleaved capture); live events continue to stream redacted lines.
- The shared limit is 8 running / 32 retained jobs, including synchronous
  starts. At capacity commands fail immediately **without spawning**, never
  wait for a slot or fall back to an unsupervised process. Diagnostics show at
  most three jobs matching this registry, run and thread; `None` is not a
  wildcard. Completed starts that never exposed a handle are observed and
  removed from both indexes immediately. Exposed handles retain normal paging
  and outcome-observation semantics.
- Accepted host input wakes omitted-yield model shell waits without cancelling
  their jobs. A shared run-incarnation handoff reservation survives the last
  shell receiver. At the post-tools boundary, `seal_for_next_request` fixes a
  sequence cut under the enqueue lock; inputs through the cut enter history
  once via `take_user_input`, while newer input stays pending. This cut does not
  close host acceptance (unlike permanent PR4b `seal`). No additional receipt
  or replay path is introduced: Delivered still requires the PR4b committed
  snapshot, and Stop / Unknown / Dropped keep that implementation's contract.
- The identical-call guard stops a run after
  `max_identical_tool_call_repeats` consecutive identical calls. Ordinary calls
  are checked by input before the batch runs. Calls whose tool reports progress
  (an explicit shell `poll` and `wait`) run and are judged by their result. A
  poll advances with new output, a status change, input that ended it, or at
  least 100 ms of real waiting; a `wait` advances with a first observed child
  completion, a question not yet reported to the caller, an agent message or
  user input. An immediate unchanged or failed result counts toward the limit,
  so repeated waits on an already finished child are still stopped. The
  call that reaches the limit has already run; its wave is recorded before the
  run stops. Each kind of call ends the other kind's streak.
  The stop reason, like the `NoProgress` one, keeps its code first and then
  names the tool and the fixed class of its last failure, e.g.
  `IdenticalToolCalls: tool=edit last_error=invalid_args; consecutive identical
  tool calls reached max_identical_tool_call_repeats=10`; `last_error` is left
  out when the last result succeeded. Inputs and error texts never enter it, so
  a parent reading a truncated reason still sees which tool looped and how.
  A job belongs to its launching run and retains its sandbox and cwd. It is not
  a durable task and cannot survive process restart.
- Shell control uses short `job-N` handles. Internal UUIDs remain in GUI events
  and result details; provider messages contain only the short handle. Numbers
  are allocated durably under the user config directory in `shell-job-handles/`
  and are shared across executors and processes. Incomplete, corrupt, or unwritable
  state fails closed: do not delete or roll back this directory to recover it.
  Restoration scans the complete saved history and compaction summaries for
  reservation hints without rewriting the provider prefix. Each `job-N` hint
  contributes only when `N + 1` is at most `u32::MAX`. Larger, overflowing, or
  unparseable numbers in quoted text are ignored individually; they neither
  block restoration nor suppress other valid hints. The allocator still rejects
  reservations above that limit unless its counter has already reached them.
  Normal allocation retains its full `u64` range, and the durable counter protects
  already-issued high numbers. Rejected reservations leave existing state
  untouched. GUI logs and cards use run, handle, and internal UUID together
  so historical instances cannot attach to another job's output.
- At most 8 running jobs / 32 retained handles per executor registry, 64 KiB live output per job, and
  an 8 MiB output artifact bound resource usage. Large output uses the existing
  private `/var/tmp` artifact facility. Truncation and the file path remain visible.
  An explicit-yield job defaults to a one-hour lifetime unless a timeout is supplied.
- Process groups are stopped and reaped before a job reports completion. There is
  no workspace lock: `write` / `edit` / `shell` never wait for a running job, in
  another run or in the run that owns it. (The run that owned a job used to have
  its mutating calls rejected until the job was polled or stopped; this was
  removed on 2026-10-15 by user decision.) The model must observe each
  terminal result before declaring completion. Run completion is published after
  process drain, final checkpoint and workspace teardown. Only then may the same
  run ID be reused; completed job handles can be released without losing persisted
  uncertainty markers. Missing cleanup confirmation keeps the workspace and reports
  failure. Snapshot failures without uncertain shell effects remain non-fatal diagnostics.
  Escalation prepares question inheritance and workspace transfer before publishing
  the source terminal event, then starts the new root in the existing event order.
- Secret filtering buffers incomplete output lines. A PTY prompt without a newline
  may not become visible until the line completes or the process exits. PTY output
  capture is bounded and cursor-based; this does not promise terminal emulation.
- `wait` accepts at most 8 related runs and `any`/`all`, and has **no deadline**
  (the agent loop drops a `timeout_ms` copied from older transcripts before
  validation; `run_output` is the non-waiting snapshot). See ADR 0042. It returns when targets finish per mode, a target
  asks a question this caller has not been told about (a wait or
  `subagent_questions` reports it), an agent message or user input arrives, or
  the caller is cancelled. Agent messages that end a model `wait` are returned
  in its `messages` field and consumed, so no `inbox` call follows; automatic
  completion notices stay queued. A `delegate` wait leaves messages queued and
  sets `inbox_ready`. The ten-minute deadline it replaces cost a turn per wake
  and stopped Orchestrators after nine identical waits on a long child; and an
  unanswered nonblocking question turned every wait into an immediate return.
  New messages, new questions and newly observed completions are progress for
  `max_no_progress_rounds`. Provider admission retains parent identity before
  registration.
- `ask_user` persists a question and immediately returns its ID; options are
  suggestions and free text is supported. The model can continue independent work.
  A required unanswered question blocks completion; answer delivery wakes the loop.
  `user_answers` reads only the run's own or explicitly inherited questions.
  Chat continuation and Worker-to-Orchestrator escalation persist explicit question
  recipients before starting the new run; failed inheritance prevents startup.
- The first answer is final; retries of the same answer are idempotent. At most 32
  questions per run and 1024 globally pending questions are permitted. Storage or
  secret-guard failures are surfaced. No answer grants tool permission.
- The GUI shows questions in their owning conversation. A storage acknowledgement
  clears an answered card even if an old run's event fence rejects its event.
  Current consumer ownership is checked; the event fence itself stays intact.

### Escalation question inheritance acceptance

The escalation question UI and answer contract supplements
[orchestration intent](../intents/evorch/features/orchestration/overview.md) and
[GUI workbench intent](../intents/evorch/features/gui-workbench/overview.md).
The completed PR #76
[packet](../.intent-cli/issues/v02-direct-escalation-handoff/packet.yaml) remains
historical evidence; these additional requirements do not rewrite that packet.

1. An inherited unanswered question appears in the destination Orchestrator
   thread with its original question ID and requester provenance unchanged.
2. An answer to that existing ID from the legitimate destination thread is
   accepted after checking current recipient / ownership. After the model
   observes the answer, the inherited blocking question no longer prevents finish.
3. The source thread shows that the question has been handed off and identifies
   the destination. It does not create a replacement question ID.
4. An answer submitted from an unrelated thread is rejected, even when it names
   a valid inherited question ID.
5. Orchestrator startup explicitly lists inherited unanswered questions and
   directs the user to answer the existing questions, not ask again with new IDs.
6. Storage restart recovery and multiple continuations preserve question identity,
   original provenance, and tracking of the current recipient / ownership.

The handoff memo must include constraints and completion criteria agreed with the
user, adopted and rejected approaches with reasons, and unverified items. These
items must not turn an unanswered question into an assumed agreement. Question
inheritance and workspace preparation still precede source termination and new
root startup; failed preparation prevents startup (ADR 0027).

### Escalation conversation and goal ownership

A root Worker escalation starts an Orchestrator in the child conversation
`escalation-{new_run_id}`. The source conversation retains its Worker root and
history. The Orchestrator and its delegates belong to the child conversation;
the source project and transferred workspace remain fixed across UI project
changes. Runtime prepares the child's ownership permit before starting work.
History replay reconstructs the conversation without acquiring write authority.
Preparation failure retains the source workspace. If provider admission fails or
is stopped, the child's memo and workspace remain available for explicit resume
with current host authority, including after restart.

An active thread goal moves to the child with the same goal ID, objective,
original request, criteria, cumulative token usage and token budget. Review and
pause settings, review rounds and findings also survive. The previous root stays
in the goal's related roots so remaining work contributes to the same budget.
Checks and in-progress review results are invalidated for the new root; a blocked
goal remains blocked. Only the child owns the transferred goal, including after
event replay and restart. Follow-up in the source conversation starts independent
Worker work rather than reclaiming the child's goal.

Public handoff events appear in this order: source terminal state,
`EscalationRequested`, the child's `ThreadGoalUpdated` when a goal exists, and
`AgentRunStarted`. The new Orchestrator begins a fresh provider context from the
handoff memo. Subsequent requests within each root retain their existing wire
prefix and prompt cache contract.

Events linking the source and child validate both ownership generations. If
acquiring a later guard encounters a writer, all previously acquired guards are
released before waiting, then every generation is revalidated. This prevents
ownership checkpoints and multi-run event validation from waiting on each
other's SQLite locks without accepting an unvalidated event.

## Shell execution environment

The shell tool describes its execution boundaries and a general diagnosis rule:
errors describe the selected environment, not the whole host. Connectivity,
filesystem access, HOME/configuration, environment variables, authentication, and
command errors require different responses. Repeating a deterministic failure in
an unchanged environment is not a new diagnosis.

| `sandbox_access` | Filesystem / HOME | Network | Environment |
| --- | --- | --- | --- |
| `isolated` (default) | Configured sandbox mounts / private HOME | Isolated | Filtered variables and explicit tool values |
| `network` | Same mounts / private HOME | Host network for this command | Same filtered environment |
| `unsandboxed` | Host filesystem / inherited HOME | Host network | evorch process environment with explicit tool overrides |

Network-only access does not restore host credentials, configuration files, or
agent sockets. Request the narrowest scope that supplies the required resources;
a host dependency already known from the task can justify requesting unsandboxed
execution directly. Network and unsandboxed commands need a nonempty justification
and review. Automatic review, user review, or disabled escalation depends on current
settings. A request does not itself establish approval. On denial, the model must
use the actual returned reason instead of guessing a GUI prompt or user refusal.

Reviewed host execution inherits native environment names and values, including
non-UTF-8 values, without copying them into command metadata or Debug output.
It inherits the evorch process, not an interactive login shell; shell startup
configuration is not loaded automatically. Unchecked DirectSandbox and delivery
paths keep their existing limited environment. Pipe, PTY, and background-job
output is filtered for known credentials and credential-shaped spans before it
is returned or written as an artifact.

A new host-supplied human request during chat restoration is fresh review evidence
and reaches delegated descendants. Restored messages, tool results, summaries, and
agent-authored delegations do not grant execution authority. Successful restoration
still reuses conversation history without rewriting the provider input prefix.

## Recovery and diagnosis

ADR 0027 defines current-authority renewal, durable tool intent before
side effects, interruption diagnosis, and the boundaries that remain unsupported.
It does not replay uncertain side effects. A durable task may seed a new run after
its actual effects have been reconciled through existing task controls.

The GUI's execution diagnostics show the last saved context, refusal reason,
interrupted tools, and related durable task. Progress distinguishes model, tools,
children, user input and compaction. Context estimates separate instructions, tool
schemas, conversation and tool output; cumulative usage remains a separate number.
The estimate is serialized UTF-8 bytes / 4, not a model-specific tokenizer or a
billing claim. Tool schemas are included consistently in compaction guards and
before/after estimates. Historical progress does not become a live timer on restart.

Sandbox preflight checks the actual launch executable and cwd. A missing sandbox
binary is reported before launch; there is no fallback to an unconfined process.
Existing per-escalation review and permission policy are retained.

## Repeatable checks

Run `scripts/check-harness.sh focused` for the deterministic reliability suite,
or `scripts/check-harness.sh full` for workspace tests, lint and optional-feature
compilation. Logs go to a newly allocated `/var/tmp/evorch-harness-check.*` directory;
the script prints its path. Remove that directory after inspection. The suite
uses scripted models and real local processes, without external model calls.

Environment-dependent ignored tests (real bwrap, display, Chromium) are not a pass
claim from this suite. Run those in their existing CI/environment jobs. The ordinary
suite covers missing executable/cwd, cancellation, storage failures, incomplete
side effects, malformed schemas, restarts, question races and ownership boundaries.

No fixed orchestration sequence, polling scheduler, blanket model approval,
unbounded log retention, provider retry layer, or new task execution engine is added.
