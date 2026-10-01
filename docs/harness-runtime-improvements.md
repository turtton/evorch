# Harness reliability improvements

Implementation branch: `codex/harness-reliability`. The operator approved the
limited restore expansion and finalization correction on 2026-09-23. The accepted
contract is [ADR 0027](../intents/evorch/decisions/0027-restore-contract.md).

## Execution and interaction

- Runtime meta tools expose concrete JSON schemas. Schema acceptance and argument
  deserialization are checked together, including roles and structured failures.
- `shell` without `yield_ms` retains synchronous execution. `yield_ms: 0..60000`
  starts a bounded background job; `action: poll | stdin | stop` uses its `job_id`
  and output cursor. A poll can wait up to 1,800,000 ms (30 minutes) for new
  output or completion; other uses of `yield_ms` remain capped at 60,000 ms.
  A job belongs to its launching run and retains its sandbox and cwd. It is not
  a durable task and cannot survive process restart.
- At most 8 running jobs / 32 retained handles per executor registry, 64 KiB live output per job, and
  an 8 MiB output artifact bound resource usage. Large output uses the existing
  private `/var/tmp` artifact facility. Truncation and the file path remain visible.
  An async job defaults to a one-hour lifetime unless a timeout is supplied.
- Process groups are stopped and reaped before releasing the workspace snapshot
  lease. Read tools may run while a job runs; conflicting writes report a useful
  error instead of waiting on the same run's lease. The model must observe each
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
- `wait` accepts at most 8 related runs, `any`/`all`, a bounded timeout (0..60 s),
  cancellation, and attention for required user questions. A timeout does not
  cancel children. Provider admission retains parent identity before registration.
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
