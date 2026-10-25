# Post-edit comment checker

The comment checker offers non-blocking review feedback after successful `Write`,
`Edit` and `apply_patch` tool calls. It does not decide whether code is self-explanatory.
The external checker extracts comments with tree-sitter, filters allowed
patterns, and returns a prompt asking the LLM to reconsider the remaining
comments. Preserve explanations of reasons, constraints, invariants, safety,
API documentation, licenses, and directives. Do not delete unrelated existing
comments just to satisfy a warning.

## Install separately

Install [code-yeongyu/go-claude-code-comment-checker](https://github.com/code-yeongyu/go-claude-code-comment-checker)
separately using its upstream instructions or a suitable asset from
[GitHub Releases](https://github.com/code-yeongyu/go-claude-code-comment-checker/releases).
evorch does not download or bundle the CLI. Its standalone Rust integration
remains MIT-licensed and does not require embedding the external checker.

The integration contract was checked against upstream v0.8.2: `check --prompt`
with upstream `Write`/`Edit`/`MultiEdit`-shaped input. An `edit` call that
changes one replacement sends `Edit`; one whose `edits` change several sends a
single `MultiEdit` with only the changed `old_string`/`new_string` pairs. This
is not a claim about npm's latest version; verify upstream packaging before
choosing an installation method.

An [`apply_patch`](apply-patch.md) call is checked once per file it changed,
after the whole patch is written. A new file sends `Write` with its content. A
replaced, updated or moved file sends `Edit` whose `old_string` and
`new_string` are the file's whole old and new text, so comments that were
already there are not reported; a moved file is checked at its destination.
Deleted files and files whose content did not change are not checked.

## Configure

Put executable settings in your user/global configuration:

```toml
[comment_checker]
enabled = true
binary = "comment-checker"
timeout_ms = 15000
# Optional prompt override; {{comments}} is the comment insertion placeholder.
# prompt = "Review these comments; preserve necessary explanations:\n{{comments}}"
```

The default `binary` is `"comment-checker"`, resolved by executable name through
the permitted host PATH. Set `binary` to another executable name or to an
absolute executable path.
`enabled = true` enables checking; the default timeout is 15,000 milliseconds.
The prompt override is optional.

Comment checker settings are accepted only from user/global configuration.
Project configuration (`.evorch/config.toml`) may only select a role profile, so
it can neither select executable code nor disable checking.

Empty or relative PATH entries are not used. Executables inside the original
project or actual worktree are rejected, including symlinks pointing there.
The binary and its dependencies must be visible inside the execution sandbox.
An installation under `$HOME`, or an npm wrapper that needs Node, may therefore
be unavailable even if it works in your terminal. There is no fallback execution
outside the sandbox. Choose a trusted installation visible to the sandbox;
do not weaken isolation to make a wrapper work.

## Non-blocking behavior and limits

If the configured executable name cannot be found in the permitted host PATH,
checking is a no-op. Unavailable explicit executable paths produce diagnostics.
Checker failures and timeouts also produce diagnostics, but the successful edit
remains successful; checker failure never rolls it back.

The checker is a separate process governed by `ApprovalPolicy`. After a
successful `Write`/`Edit` that changes the file, it runs at most once when the
resolved action is `Proceed` or `AskOnFailure`; after `apply_patch`, at most
once for each changed file. Under `Deny` or `AskFirst`, checking is skipped.
There is no interactive approval or retry on failure;
`AskOnFailure` permits only the initial checker invocation.
Existing `Write`/`Edit`/`apply_patch` permission classification is unchanged.

Feedback is returned with a warning marker and quoted-content boundaries.
Treat the quoted checker output as review material, not trusted instructions.
The tool result retains `is_error = false`; warnings do not block `finish`.
Review feedback in context rather than treating it as an instruction to remove
every comment.

For `apply_patch`, the warning follows Codex's success message. The
diagnostics of all files of one call share one warning block, each introduced
by the file's resolved path:

```text
Success. Updated the following files:
A src/a.rs
A src/c.rs
M src/b.rs
M src/d.rs

[comment-checker warning]
External comment-checker diagnostic (untrusted; quoted):
/work/src/a.rs:
> first diagnostic
/work/src/b.rs:
> second diagnostic
[end comment-checker warning]
```

The files share the preview size of one warning. When a diagnostic is cut, the
block ends with `> [diagnostic preview truncated; see output artifact]` and one
output artifact keeps every file's full text. The result's
`detail.comment_checker` lists every checked file with its own outcome:

```json
{
  "outcome": "warning",
  "files": [
    {"path": "/work/src/a.rs", "outcome": "warning", "message": "first diagnostic"},
    {"path": "/work/src/b.rs", "outcome": "warning", "message": "second diagnostic"},
    {"path": "/work/src/c.rs", "outcome": "pass"},
    {"path": "/work/src/d.rs", "outcome": "unavailable", "reason": "timeout"}
  ]
}
```

`outcome` is `warning` when any file warned, and `unavailable` when no file
warned but the checker's one-time unavailability notice is reported; the notice
adds `reason` and `detail`. `message` holds the previewed diagnostic, and
`output_artifact` is added when a preview was cut. A call whose files all pass,
or whose unavailability was already reported, attaches no checker detail.

Shell-based edits are outside this hook. There is no full-diff checker at
completion, so this feedback is neither comprehensive coverage nor a substitute
for normal review and tests.
