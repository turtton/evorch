# Harness output, context and recovery

## File tools and working directory

- `read({path, offset, limit})` reads up to 300 lines / 16 KiB. `offset` starts
  at 1. The response gives continuation arguments; `byte_offset` handles a
  single line larger than the byte limit without splitting a UTF-8 character.
- `write({path, content})` creates or fully replaces a file. `edit` replaces a
  nonempty `old_string` in an existing file. Empty search text is rejected.
  Without `replace_all: true`, `old_string` must match exactly one location,
  counting matches that overlap each other (`aba` matches `ababa` twice);
  several matches fail with their count and line numbers and leave the file
  unchanged. `replace_all: true` replaces non-overlapping matches from left to
  right, like `str::replace`: replacing `aba` with `X` turns `ababa` into `Xba`.
  `edits: [{old_string, new_string, replace_all?}]` applies several
  replacements to one file in one call. Every `old_string` is matched against
  the original contents, replaced ranges must not overlap, and either all are
  written or none; errors name the failing `edits[i]`. A call sends either
  `old_string`/`new_string` or `edits`, not both. In a file whose line breaks
  are all CRLF, LF line breaks in `old_string` and `new_string` are treated as
  CRLF. In a file that mixes CRLF and LF, a multi-line `old_string` written with
  LF is matched as CRLF only when nothing matches as written, and its
  `new_string` is then written with CRLF too. Other bytes, including a byte
  order mark, are left as they are. An edit that leaves the contents unchanged
  reports `No changes` without rewriting the file. Prefer `edit`/`write` to
  shell `sed -i`, heredocs or scripts: only they return the diff and run the
  post-edit comment checker.
- Relative file paths and shell `cwd` (including `.`) resolve from the run's
  workspace. File tools do not change the application's process directory.

## File-tool path boundary

`read`, `grep`, `write` and `edit` run in the evorch process, but without
escalation they reach only what the run's sandboxed shell sees (ADR 0033). One
list, `sandbox::SandboxBoundary`, is assembled per run in
`runtime::network::sandbox_boundary` and drives both the bwrap mounts
(`BwrapConfig::from_boundary`) and the file-tool check
(`ToolExecutor::set_file_boundary`), so a region added to one is never missing
from the other. All regions are host paths:

- read-write: the project root and this thread's `$TMPDIR` scratch;
- read-only: `/usr` `/bin` `/lib` `/lib64` `/etc` `/nix`, the tool output store,
  and the Cargo (`registry/{index,cache,src}`, `git/{db,checkouts}`, `bin`) and
  rustup (`toolchains`, `settings.toml`) caches under the host `CARGO_HOME` /
  `RUSTUP_HOME` (not the `/tmp/home` paths the shell sees them at). Cargo
  credentials and config stay outside.

Allowed directories (ADR 0032) join the same list (see below). When regions
overlap, the last mounted one decides, as in bwrap. `read` and `grep` may use every region; `write` and `edit` need a
read-write one. Paths are checked after relative ones are resolved against the
workspace: a path with `..`, a dangling link, or an ancestor that cannot be
resolved is outside; otherwise the deepest existing ancestor is canonicalized,
so a link inside the project that points out is outside, and a link outside
that resolves into the project counts as inside. Sockets, FIFOs and devices are
refused even inside. `grep` runs `rg` without `--follow`, so a search started
inside never returns content through links that leave the boundary. As with the
benchmark guard, the check assumes no concurrent untrusted process swaps links
between the check and the open.

A call outside the boundary is denied before the tool runs, so no file content
reaches the result or events. The denial suggests both remedies: retry once with
`sandbox_access: "unsandboxed"` and a `justification`, or ask the user to
register the location as an allowed directory. `sandbox_access` defaults to
`"isolated"`, meaning "inside the sandbox boundary"; it is unrelated to the
isolated workspaces removed by ADR 0031. File tools have no `"network"` value.

`"unsandboxed"` requires a nonempty justification and is reviewed on every call
by the same gate as shell escalation, under `sandbox.escalation_approval` and
`escalate_to_user_on_deny`: `off` denies every outside access, `user` asks the
user (approval input `kind: "file_access"` with the tool, canonical `path`,
`access`, justification and change), and `auto` asks the escalation reviewer,
falling back to the user on a decline when configured. An approval the
reviewer rates critical-risk always goes to the user for the final decision,
whatever `escalate_to_user_on_deny` says. When the automatic reviewer denies
three consecutive shell or file escalation requests in one run, that run's
later requests are refused
without review (and without asking the user) until the user sends new input to
the conversation. Policy denials count, including model `approve: false`, approvals
converted to denials for insufficient authorization, and missing-real-user-request
denials returned without calling the model. Model failures, timeouts, invalid
verdicts, unsynchronized evidence, settings/context changes during review, and
human refusals do not count. Settings/evidence changes during review neither add
a denial nor reset the streak, regardless of the final model or human verdict.
Evidence that was already unsynchronized at the start, but remains unchanged,
does not by itself invalidate human approval in `user` mode or `auto` fallback;
automatic review still rejects unsynchronized evidence.
A valid final approval in any approval mode resets the streak.
Only new real-user input visible in the run's lineage lifts suspension, including
host-submitted answers, not parent-agent answers or delegated prompts. Delayed
results from an older input generation cannot change the current generation.
Before handing off to human approval, even an older pending review checks the
current generation's suspension: new input may have lifted the old suspension
or the newer generation may already have reached the limit again. The suspension
check and denial recording share one mutex; that check is the handoff-admission
linearization point, not the later `ApprovalRequested` event. A handoff admitted
before the third denial may publish its dialog afterward. A check ordered after
the threshold is reached refuses without calling the user, for both critical
verdicts and deny fallback. The mutex is released before waiting for approval.
Audit and diagnostic reasons contain only the refusal facts, limit, and reset
condition; workaround/consultation guidance and the reminder that sandbox work
can continue appear only in agent-facing tool results.
The per-call reviews of Web fetches, navigation and uploads are outside this count.
The reviewer and user
see the tool, the canonicalized target paths and the justification, plus for
`write` the first 4000 bytes of the content and for `edit` every edit with its
`edits[i]` index and `replace_all` flag. Only the old/new strings are cut: they
share 4000 bytes, so a long string never hides a later edit or its scope.
Existing outside files are not read for the review.
Approval covers that one call and lifts only the path boundary: role
capabilities still decide which tools a run can call, so a read-only role
cannot write through escalation.
Benchmark runs keep their fixed workspace guard, drop `sandbox_access` and
`justification` from the file-tool schemas, and refuse `"unsandboxed"`.

## Allowed directories

A project's allowed directories reach its runs only once trusted (ADR 0032).
The GUI declares, per project root, the `Approved` directories that currently
exist (`AgentRuntime::set_project_allowed_directories`), and declares again
after every add, Trust, Untrust or Remove in the project settings, and before
runs start. Each declaration replaces the project's list. The runtime keys the
list by the canonical project root, canonicalizes each directory, and skips
(with a warning) any that is missing or not a directory, so a run never fails to
start because of one. A run uses only the list of its own project.

Access follows the run's role: a role whose tools include `write`, `edit` or
`shell` (today only Worker) gets them read-write; every other role read-only,
both in its shell and in its file tools. The project root stays read-write for
every role.

Runs hold the project's live list rather than a copy. bwrap builds its argv for
each command from the list at that moment (`--bind-try` / `--ro-bind-try`, so a
directory deleted in between is tolerated), and the file-tool check lists its
regions again on every call. A change therefore applies from the running run's
next command or file tool call; a background shell job keeps the mounts it was
started with until it exits.

Regions are mounted, and listed for the file tools, in this order, so later ones
win where they overlap: system read-only paths, the `/tmp` alias of the scratch,
allowed directories, the scratch at its host path, the tool output store, the
build caches, then the project root. The `/tmp` alias comes first so that an
allowed directory under the host `/tmp` is not hidden by it.

Adding refuses `/` and the home directory itself (after canonicalization),
paths inside the project root, and paths inside another allowed directory. A
saved directory that no longer exists keeps its record and trust: the sidebar
still loads, the directory is just not declared until it exists again, and it
can still be untrusted or removed.

### What runs are told

A run's system message lists the allowed directories it started with and the
thread read-only directories it mounted (ADR 0037), in an `Allowed directories
(evorch): ` section right before the workspace note (the inspector shows the
same section). Access labels are this run's own permissions, and entries stay
short, for example `* /srv/shared (read-only)`. An empty list adds no section.
The section is never rewritten, not even on restore, so a change never
invalidates the sent prefix. Delegated children start with the list current at
their own start. Runs see the list whether or not they are sandboxed; benchmark
runs see none.

A run that can delegate, and whose delegated Workers would get different
access, gets one extra sentence per message instead of a note on each entry.
Only then does the message also say that the labels are **this run's own
permissions**; Workers, runs that cannot delegate, and runs whose Workers get the
same access everywhere (for example a list entirely covered by thread read-only
directories) get neither. The paths are not repeated: either every listed
directory has the stated access for Workers, or the sentence names only the
exceptions. For example, a read-only Orchestrator's section can say:

```text
Allowed directories (evorch): besides the workspace, you may use these directories. Access labels describe this run's own permissions. Delegated Workers have read-write access to all of them except `/srv/drafts`, which stays read-only for them too. Changes arrive later as `[workspace update]` messages.
* /srv/drafts (read-only)
* /srv/shared (read-only)
```

The sentence describes a Worker delegated now. Such a Worker mounts the
thread's current declaration, while the run keeps the mounts it started with,
so the run follows the declaration at every request boundary (it is checked
again only when it changes). Thread read-only directories, and allowed
directories equal to or inside them, are read-only for that Worker too and are
named as exceptions. A thread read-only directory the run mounted but whose
declaration was withdrawn is writable for the Worker inside an allowed
directory, readable inside a path every Worker sandbox mounts read-only (system
paths such as `/usr`, Cargo/rustup caches), and otherwise not mounted for it
(``... except `/srv/ref`, which they cannot access.``). A directory declared
since the run started inside a listed one, where the Worker gets other access
than around it, joins the list with the run's own access there, so the sentence
can name it (an Orchestrator gets `+ /srv/shared/locked (read-only)`).
Exceptions nested in an entry where the Worker gets the same access are folded
into it. Without exceptions the sentence ends at `all of them.`

Changes are appended instead. After a complete tool round, right before the
next provider request (never between a tool call and its result), a run whose
list differs from the one it was last told gets one message:

```text
[workspace update] Allowed directories changed (+ added, - removed, ~ access changed):
+ /srv/shared (read-write)
- /srv/old
```

A read-only Orchestrator whose Workers may write the added directory gets the
same message with the sentence for the lines with an access label (`+` and
`~`) instead of the colon:

```text
[workspace update] Allowed directories changed (+ added, - removed, ~ access changed). Access labels describe this run's own permissions. Delegated Workers have read-write access to every added or changed directory.
+ /srv/shared (read-only)
- /srv/old
```

A message with only removals has no sentence. A change only to a Worker's
access, such as a thread declaration newly covering an allowed directory or
withdrawn from it, restates the whole list instead of emitting a misleading
`~ ... (read-only, was read-only)` line. So does a removal after which the
entry around the removed one gives Workers other access than they were told for
it, such as withdrawing `/srv/shared/locked` from the declaration: a bare
`- /srv/shared/locked` would not say that Workers can write there again. Once a
run was told that Workers get different access, a later message whose entries
do not differ says
`Delegated Workers have the same access to all of them.` (or `to every added or
changed directory.`), so the earlier sentence does not stand. A whole list
supersedes earlier sentences, so after one without a difference nothing more is
said; a change leaves the earlier sentence standing for the entries it does not
label, so each later change whose labelled entries do not differ says it again.
The last told list, including any differing Worker access and whether such a
sentence still stands, is saved in the restore snapshot. Older snapshots without that
information still load; a newly learned difference triggers one full
restatement. Ordinary restore otherwise tells only what changed while stopped;
an interrupted request is first retried with its exact saved input. Restore,
chat restore and fork keep the saved System rather than rebuilding its allowed
section.

Compaction (including model-switch compaction) and a fork replace history, so
the next boundary restates the whole current list as `[workspace update]
Current allowed directories; ...`, unless that would only repeat the initial
System. Compaction never summarizes the leading System and a fork keeps it. The
restatement is skipped when that System carries exactly the section for the
current list and the raw history (which keeps messages a checkpoint summarized,
and for a fork ends at the cut) holds no directory update. A changed list, an
older System wording, a missing System section, or any directory update in the
history, even one later reverted, still causes one full restatement, since an
old update or its summary could contradict System. When restating, a run that
once had directories but has none left is told `none`; runs that never had any
get no extra message.

### Instructions inside allowed directories

Rules and repository skills of an allowed directory never enter the system
message. They are read the first time a run touches the directory: the `path`
of a successful `read`, `grep`, `write` or `edit`, or the `cwd` of a
successful `shell` (paths inside shell commands are not detected). Targets are
resolved through symlinks, and anything inside the project root keeps the
project's own rules even when an allowed directory contains the root. After
the tool round, right after its last result and separately from project rules,
the run gets one message:

```text
[allowed-directory instructions] /srv/shared
## rules: /srv/shared/AGENTS.md
...
Repository skills here (load with skill_load, or pass to delegate load_skills):
- deploy: Deploy steps
```

The directory is the root of its own rule search, from the directory down to
the target, within the `max_injection_bytes` budget. Each rule file is appended
once per run; one left out by the budget is tried again on a later touch. The
skills are listed on the first touch and become loadable with `skill_load` or
delegate `load_skills` in that run only; the shared catalog and the system
message stay as they are, and a name another skill already uses is listed as
not loadable. When the directory leaves the list, loading its skills fails with
the reason and nothing more is appended for it.

What was appended and which directories' skills were loaded are saved in the
restore snapshot, so a restored run does not get them again. Compaction
(including a model-switch compaction) and a fork replace history, so the next
touch appends the directory's instructions once more; loaded skills stay
loadable.

## Temporary output

Shell pipes and PTYs are drained into bounded buffers (8 MiB per stream).
After completion or timeout, credential-shaped text is redacted before any
artifact is written. This keeps raw credentials out of temporary files without
requiring an unbounded streaming-redaction buffer. Small completed shell results
and live shell job lines are filtered as well. Source files remain unchanged;
persisted event/context copies are sanitized.

Large results return the last 300 lines / 16 KiB plus an artifact reference.
Artifacts live in private `/var/tmp/evorch-output-<uid>/<slot>/<uuid>.txt` files,
are mounted read-only into sandboxed shells, and can be inspected with `read`
or `grep`. Set `EVORCH_OUTPUT_DIR` to a nonempty absolute base directory to use
`<base>/evorch-output-<uid>/<slot>/<uuid>.txt` instead. An unset or empty value
keeps `/var/tmp`; relative paths are rejected, and `TMPDIR` is never consulted.
Missing base directories are created recursively. The per-user output directory
is still created with mode 0700 and checked for current-user ownership, no group
or other permissions, and no symlinks, including with a custom base.
Sandboxed commands get `EVORCH_OUTPUT_DIR` set to their writable scratch unless
the command sets it explicitly, so an evorch built and tested inside the sandbox
does not try to write into the read-only host directory.
If the capture limit is exceeded, the result explicitly reports that
only the initial portion was saved, and previews the end of that saved portion.
Timeout is an error result containing the captured partial output.

There are 64 slots shared across processes and restarts, with at most 8 MiB
per artifact (512 MiB total, excluding small lock/directory metadata). New
artifacts replace one slot at a time under a file lock. Paths contain unique
IDs so an expired reference cannot point to an unrelated output. A background
worker checks one slot per minute for files older than seven days; there is no
startup scan or bulk deletion on the UI thread. Expired files are optional
history aids, never a requirement for conversation restoration.

Shell temporary files live in one scratch directory per thread (ADR 0033):
`/var/tmp/evorch-scratch-<uid>-<digest of the ownership root>/<thread>/`. The
key is the thread bound by `bind_thread_root`, or the root run ID for a root
run with no thread, so a thread's root run and all its child runs share the
directory and its path survives restores and root rebinding. The parent is
created lazily with mode 0700 and must be owned by the current user; a
mismatched owner, mode or a symlink fails the run's sandbox setup.

Inside bwrap the directory is mounted read-write at its own host path and at
`/tmp`, and `TMPDIR` is the host path. Shells escalated to `unsandboxed` get
the same `TMPDIR`, and the file tools already resolve that host path, so
`$TMPDIR/<file>` names one file everywhere. `/tmp` and `~` still differ
between the shell and the file tools. HOME stays at `/tmp/home` inside the
scratch: Cargo registry/git caches and installed tool binaries are mounted
read-only there, and host Cargo credentials/config are excluded. Network
isolation remains controlled by the existing policy. A sandbox built without
a scratch directory (benchmarks, and executors shared by runs of a runtime
without a sandbox root) falls back to a private directory of its own and
adds no system-message block.

Contents last only while this process owns the thread (ADR 0024). Starting or
claiming a thread empties its directory before use; releasing it or handing
it off deletes the directory. Opening an ownership host deletes discarded
trees, directories of threads without a live owner (unknown liveness keeps
them), and unbound-run directories when no other owner process may be live.
Runtimes without an ownership host use a private parent removed with the
runtime. Removal renames the tree aside immediately, then a bounded background
queue deletes it with throttled, symlink-safe traversal; nothing scans or
deletes on the UI thread.

New runs carry a `Temporary files (evorch): $TMPDIR is <path>.` block in their
system message, separate from the workspace note and never rewritten on
restore. A restored run whose system message lacks that block, or names
another path, gets the same text appended once before its next model request
instead; a restored run whose directory was emptied by a claim gets a one-time
"was emptied" notice. Both are appended after the already-sent prefix and wait
for an interrupted request to replay its exact saved input first.

## Token accounting and compaction

Canonical `input_tokens` includes cached reads and cached writes. Cache counts
are subsets used for cost calculations, not additional context or budget usage.
OpenAI-compatible/Kimi input already has this total. Legacy Kimi root-level
`cached_tokens` is a fallback when nested cache details are absent. Anthropic
input is normalized from uncached input + cache read + cache creation.

Context pressure projects the next request from the provider's reported input
and output usage plus an estimate for messages added since that response. The
cumulative input/output totals remain checkpoint telemetry; they do not stop a
run by default. An operator may explicitly set `[budget].max_tokens` as a
separate run-level usage cap. Cached input counts once in that optional cap.
Price estimates apply the provider's separate cached rates. Without reported
usage, serialized UTF-8 bytes / 4 remains a heuristic rather than a
model-specific tokenizer. Codex context limits come from its `/models`
response when available, followed by models.dev and the configured fallback.

Old bulky tool results are replaced by saved artifact references, retaining
call IDs and error status. The eight most recent results remain visible.
Summaries stream through a private event bus, without mixing summary text into
the chat. Model summaries default to 90 seconds of stream inactivity and a
600-second overall deadline, including provider retries. Official Codex
compaction uses a dedicated HTTP client with a 600-second overall deadline and
a 300-second network-read timeout. Ordinary provider requests retain their
90-second network-read timeout. Failed summaries back off for 4, 8, 16, ...
turns. If context still reaches the model window, the run stops with a
recoverable checkpoint.

The following configuration fields are exposed (values shown are defaults):

```toml
[budget]
max_elapsed_secs = 7200

[compaction]
summary_idle_timeout_secs = 90
summary_timeout_secs = 600
failure_cooldown_turns = 4
```

## Recovery

A structurally valid checkpoint is saved after completed tool batches and at
run start. Credential-shaped spans in message payloads are replaced in the
persisted copy, preserving roles and tool-call IDs. Terminal persistence errors
emit a diagnostic and retain any previous valid checkpoint. An incomplete tool
batch is excluded from terminal snapshots. Restoration does not open output
artifact paths; missing temporary files do not prevent continuation.


## Interactive execution and recovery diagnostics

See [the runtime improvements](harness-runtime-improvements.md) for bounded shell
jobs, durable user questions, event-driven waits, context composition, the restore
contract proposal and repeatable regression commands. Pending tool intent is now
saved before dispatch, so an incomplete side effect blocks automatic restoration
instead of silently repeating a tool excluded from the terminal history.
