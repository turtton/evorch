# apply_patch

`apply_patch` is a Codex-compatible editing tool. One patch can add, update,
delete and rename several files. The parser and the application engine are
ported from [OpenAI Codex](https://github.com/openai/codex) (revision
`7c568b33`); `crates/apply-patch/UPSTREAM.md` lists the ported files and every
intentional difference. Like `write` and `edit`, only the Worker role may use it.

## Choosing the editing tool

Each model is shown one editing interface: either `apply_patch`, or `edit` and
`write`. The `edit_tool` key selects it, with the values `"apply-patch"` and
`"edit"`. It can be set on a provider profile and on a model entry written as a
table:

```toml
version = 3

[providers.openai]
type = "openai"
api_key_env = "OPENAI_API_KEY"
models = [
  "gpt-5.5",                                           # default: apply-patch
  { id = "gpt-5.4", enabled = true, edit_tool = "edit" },
]
default_model = "gpt-5.5"

[providers.local]
type = "openai-compatible"
base_url = "http://localhost:8000/v1"
api_key_env = "LOCAL_API_KEY"
edit_tool = "apply-patch"                               # every model of this profile
models = ["gpt-oss-120b", { id = "qwen3-coder", enabled = true, edit_tool = "edit" }]
default_model = "gpt-oss-120b"
```

The first matching rule wins:

1. The model entry's `edit_tool`. A `+fast` ID uses its own entry when one
   exists, and otherwise the entry of the ID without `+fast`.
2. The profile's `edit_tool`.
3. The default: `apply-patch` when the provider type is `openai` or
   `openai-codex` and the model ID is `gpt-<N>` with N ≥ 5, where N is the
   number right after `gpt-` (for example `gpt-5`, `gpt-5.1-codex`,
   `gpt-6-astra`). The `gpt-` prefix is matched case-insensitively, so `GPT-5`
   counts too. Everything else defaults to `edit`: IDs that do not start with
   `gpt-` (`o3`, `codex-mini-latest`), IDs without a number after it
   (`gpt-oss-120b`), older generations (`gpt-4.1`), and every model of any
   other provider type.

Other values are rejected when the configuration is loaded, with the
`providers.<profile>.edit_tool` or `providers.<profile>.models[<i>].edit_tool`
path. The [configuration reference](../intents/evorch/operations/config-reference.md)
describes the key as well.

The selection only narrows what a request shows. All three tools stay
registered and authorized, a role without file-editing capability never gains
one, and the base context preview lists the hidden tools with the reason
("Replaced by apply_patch for this model" or "This model uses edit and write
instead of apply_patch").

## What the model sees

A model selected for `apply_patch` sees `apply_patch` and neither `edit` nor
`write`. Every other model sees `edit` and `write` and not `apply_patch`.

- **Responses API** (`openai`, `openai-codex`): `apply_patch` is a freeform
  custom tool with Codex's Lark grammar and description. The model writes the
  patch itself, not JSON. The call streams as `custom_tool_call` input and its
  result is returned as `custom_tool_call_output`.
- **Chat Completions and Anthropic Messages** (`openai-compatible`, `anthropic`
  and the other providers that send tools as JSON functions): `apply_patch` is a
  JSON function with a single string argument, `input`, which holds the whole
  patch from `*** Begin Patch` to `*** End Patch`.

Both forms take the same patch:

```text
*** Begin Patch
*** Add File: docs/hello.txt
+Hello, world!
*** Update File: src/app.py
@@ def greet():
-    print("Hi")
+    print("Hello")
*** Update File: src/old_name.py
*** Move to: src/new_name.py
@@
-OLD = 1
+NEW = 1
*** Delete File: obsolete.txt
*** End Patch
```

A successful call answers like Codex, with the paths as written in the patch:

```text
Success. Updated the following files:
A docs/hello.txt
M src/app.py
M src/new_name.py
D obsolete.txt
```

## History and model switches

Earlier `apply_patch` calls stay in the history in the wire's own form while the
editing interface stays the same: Responses requests replay them as
`custom_tool_call` and `custom_tool_call_output` items, and Chat Completions and
Anthropic requests as function calls with `{"input": ...}`. Ordinary replay
never rewrites them, so the cached prompt prefix is kept.

Moving a run to a member or model with the other editing interface is a model
switch boundary, like any change of member (see
[Compaction across a switch](providers.md#compaction-across-a-switch)), even
when the profile and model family stay the same. This applies to chain
fallback, an explicit model change, and a settings reload that changes the
selected profile/model's `edit_tool` override.

Before the first agent request using the new interface, the runtime summarizes
the old history as text, **whether or not a checkpoint exists**. This uses the
existing model-switch re-summary path and reports a compaction with reason
`ModelSwitch`. It includes earlier editing calls and their results, including
calls after an existing checkpoint: old `apply_patch` or `edit`/`write` calls
are not replayed raw to the new agent interface. Opaque Codex state is replaced
from its covered raw messages, even if the new member could replay the blob.
The latest plain user input and system instructions remain intact. The
model-switch re-summary request advertises no tools, so its tool-call/result
evidence is rendered as text (including call names, arguments, IDs, output, and
error status), not as wire tool-call/result items. This conversion applies only
to model-switch re-summary input: ordinary manual/automatic compaction, normal
agent replay, and stored raw history remain unchanged.

Each fallback that changes the editing interface adds one summary invocation
before the next agent request (and its model-call cost when the model
summarizer is used). Same-interface fallback does not add this re-summary;
ordinary context-window compaction still applies. A run's first request with
no earlier history does not summarize.

Settings reloads retain the last editing interface per run/chain. After a
restart or resume where that record is unavailable, an opaque checkpoint is
re-summarized conservatively. Without a checkpoint, an earlier `apply_patch`,
`edit`, or `write` tool call triggers re-summary only when the selected
interface cannot use that tool; otherwise no re-summary is needed. The on-disk
checkpoint carries an optional `protects_history = true` marker for
model-switch re-summaries and compactions derived from them. Later window,
manual, and automatic compaction use that checkpoint's projected history, not
the hidden raw tool batches, and keep its replacement range covered. Ordinary
checkpoints omit the marker; their serialized bytes and summary input are
unchanged. A request also checks visible editing calls even when an interface
record exists, so a failed/interrupted re-summary cannot be bypassed on resume.
The interface record is committed only after preparation succeeds; this also
keeps opaque-checkpoint re-summaries pending after failure. If the protected
summary itself is too large for a target window, compaction summarizes that
safe projected summary again instead of restoring its raw tool batches.
Later requests preserve and reuse the new post-summary prefix.

## Approval policy

`apply_patch` writes files, so it has its own `ApprovalPolicy` decision.
Overrides that name `edit` or `write` do not apply to it. Name it explicitly,
or use the `*` override:

```rust
use sandbox::{ApprovalMode, ApprovalPolicy, PolicyDecision};

let policy = ApprovalPolicy::standard(ApprovalMode::OnRequest)
    .with_override("edit", PolicyDecision::AutoAllow)
    .with_override("write", PolicyDecision::AutoAllow)
    // Without this line apply_patch keeps the standard decision for file writes, Ask.
    .with_override("apply_patch", PolicyDecision::AutoAllow);
executor.set_policy(policy);
```

The post-edit comment checker is a separate process with its own decision; see
[Post-edit comment checker](comment-checker.md).

## Paths, the sandbox boundary and team workers

Relative paths resolve against the run's workspace, never against the process
directory. Without a workspace, relative paths are rejected. A workspace given
as a relative path, such as `evorch run --project .`, is made absolute against
the process directory once, when it is set. A path containing `..` is always
rejected.

Every path of a patch goes through the file boundary before anything is read or
written: added, updated and deleted files, and both the source and the
destination of a move. When one of them is outside the boundary, or in a
read-only part of it, the whole call is denied and nothing changes. Unlike
`read`, `write` and `edit`, `apply_patch` has no `sandbox_access` argument and
cannot be escalated. If the model needs to change files elsewhere, ask the user
to register the location as an allowed directory of the project.

The boundary checks both the resolved content path and the directory entry
(canonical parent plus filename). Delete and move sources need write access to
the entry and read access to the referent; add, update and move destinations
need write access to both. Content writes follow a final symlink and atomically
replace its referent with a same-directory temporary file; they do not replace
the symlink. Deleting or moving a symlink unlinks the symlink itself, not its
referent. Symlink aliases share virtual contents and produce one final change
per content destination; the success summary still lists the raw patch paths.
Unlike `apply_patch`, existing `edit`/`write` atomically replace the named entry
itself, so a final symlink becomes a regular file. Their boundary checks also
require the entry to be writable, in addition to their existing referent check.

A move whose destination resolves through the source entry is treated as an
update without unlinking the source. For example, moving `source` to
`alias -> source` updates `source` and preserves `alias`; unlinking `source`
would otherwise discard the result and leave `alias` dangling. This also
applies to a destination symlink chain that passes through a source symlink.
Distinct source and destination symlinks that independently point to one
referent still move normally: only the source symlink is unlinked. In all
cases, the summary keeps the raw move destination from the patch.

Team workers may only patch paths owned by their live task claims. The team
guard checks every path of the patch in the same way, including move sources
and deletions. A single path that the worker does not own, or that is outside
the workspace, rejects the whole call before any file changes. A patch that
does not parse is passed to the tool, which rejects it without changing
anything. The guard judges the `path` of team workers' `write` and `edit`
calls the same way: relative paths resolve against the run's workspace,
absolute paths must lie inside it, and without a workspace, or with one that
is still relative, the call is rejected. For both, symlinks are looked for
between the workspace root and the file, not below the process directory, and
a path through a symlink is rejected.

## Concurrency

`apply_patch` runs exclusively: when a model requests several tool calls at
once, an `apply_patch` call in that batch runs alone. This only orders the
calls of one run. It is not a lock between runs or processes. Files are not
locked, and a change made by another run, a shell command or an editor between
checking and writing the patch is not detected; the later write wins. Do not
let two runs change the same files at the same time; team task claims divide
paths among workers.

This is also a TOCTOU limitation of the boundary checks: a process that swaps a
symlink or ancestor directory after validation can redirect a later read,
unlink or atomic replacement outside the checked boundary. These checks do not
guarantee safety against non-cooperating processes that can change filesystem
links or directories concurrently. Serialize such filesystem changes across
validation, planning and commit; per-run exclusive execution is not sufficient.

## Failures and partial application

Before anything is written, every change of the patch is applied to an
in-memory copy of the files. When the patch does not parse, names a path that
cannot be used, does not match the current contents, or reads a file that is
not UTF-8 text, no file changes. These failures start with
`apply_patch verification failed: `. A patch without changes reports
`No files were modified.`

Writing is not a transaction across files. If writing fails partway, for
example because a file cannot be replaced, the changes already written stay and
are not rolled back. The error is followed by the list of those changes:

```text
Failed to write file /work/src/b.rs: Permission denied (os error 13)
Applied before the failure:
A src/a.rs
```

The result's `detail.apply_patch.error_class` is `parse`, `match`, `io` or
`path`, and `detail.apply_patch.committed` lists the written changes.

With `ApprovalMode::OnFailure`, a failed call can be approved and is then run
again with the same patch. After a verification failure nothing changed, so
the second run fails the same way unless the files changed in the meantime.
After a partial write, the second run starts from the partly patched files.
Changes that were already written usually no longer match, so the second run
is rejected before writing anything; if every change still matches, for
example when only added files were written, the patch is written again and
those files are overwritten. Re-read the files and send a new patch for the
remaining changes rather than approving the same patch again.

## Benchmarks

A benchmark checkpoint stores the Worker's tool schemas and their order. Adding
`apply_patch` changed that list, so checkpoints recorded by earlier versions
no longer match, and replay reports them as unsupported ("available tool
schemas/order differ from checkpoint"). Record them again. During replay, the
benchmark instructions list only the editing tools that the frozen candidate
model sees.
