# Harness output, context and recovery

## File tools and working directory

- `read({path, offset, limit})` reads up to 300 lines / 16 KiB. `offset` starts
  at 1. The response gives continuation arguments; `byte_offset` handles a
  single line larger than the byte limit without splitting a UTF-8 character.
- `write({path, content})` creates or fully replaces a file. `edit` replaces a
  nonempty `old_string` in an existing file. Empty search text is rejected.
- Relative file paths and shell `cwd` (including `.`) resolve from the run's
  workspace. File tools do not change the application's process directory.

## Temporary output

Shell pipes and PTYs are drained into bounded buffers (8 MiB per stream).
After completion or timeout, credential-shaped text is redacted before any
artifact is written. This keeps raw credentials out of temporary files without
requiring an unbounded streaming-redaction buffer. Smaller live results and
source files remain unchanged; persisted event/context copies are sanitized.

Large results return the last 300 lines / 16 KiB plus an artifact reference.
Artifacts live in private `/var/tmp/evorch-output-<uid>/<slot>/<uuid>.txt` files,
are mounted read-only into sandboxed shells, and can be inspected with `read`
or `grep`. If the capture limit is exceeded, the result explicitly reports that
only the initial portion was saved, and previews the end of that saved portion.
Timeout is an error result containing the captured partial output.

There are 64 slots shared across processes and restarts, with at most 8 MiB
per artifact (512 MiB total, excluding small lock/directory metadata). New
artifacts replace one slot at a time under a file lock. Paths contain unique
IDs so an expired reference cannot point to an unrelated output. A background
worker checks one slot per minute for files older than seven days; there is no
startup scan or bulk deletion on the UI thread. Expired files are optional
history aids, never a requirement for conversation restoration.

Sandbox `/tmp` is a private disk directory under `/var/tmp`, shared across
commands using the same sandbox. Cleanup uses a bounded background queue and
throttled directory traversal; crash leftovers are left to OS temporary-file
cleanup. Cargo registry/git caches and installed tool binaries are mounted
read-only into an isolated HOME; host Cargo credentials/config are excluded.
Network isolation remains controlled by the existing policy.

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
the chat. Defaults are 90 seconds of stream inactivity and a 300-second overall
summary deadline. Failed summaries back off for 4, 8, 16, ... turns. If context
still reaches the model window, the run stops with a recoverable checkpoint.
The provider HTTP layer also has a 90-second network-read timeout.

The following configuration fields are exposed (values shown are defaults):

```toml
[budget]
max_elapsed_secs = 7200

[compaction]
summary_idle_timeout_secs = 90
summary_timeout_secs = 300
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
