# Upstream

This crate ports part of `codex-rs/apply-patch` from OpenAI Codex
(<https://github.com/openai/codex>, Apache-2.0; see `LICENSE-APACHE` and
`NOTICE`).

- Revision: `7c568b331b9e0810ae7164f4bd680cec5f69f2fc`
- Raw files: `https://raw.githubusercontent.com/openai/codex/<revision>/<path>`

## Ported files

| Upstream path | Local path | Notes |
|---|---|---|
| `codex-rs/apply-patch/src/parser.rs` | `src/parser.rs` | Parser and its tests. `ParseError` lives in `src/error.rs`. |
| `codex-rs/apply-patch/src/streaming_parser.rs` | `src/streaming_parser.rs` | Verbatim apart from imports; tests included. |
| `codex-rs/apply-patch/src/seek_sequence.rs` | `src/seek_sequence.rs` | Matching algorithm unchanged; corrected EOF documentation and added boundary tests. |
| `codex-rs/apply-patch/src/text_file.rs` | `src/text_file.rs` | Line-ending-preserving text representation and replacements. |
| `codex-rs/apply-patch/src/file_update.rs` | `src/file_update.rs` | Replacement computation unchanged; input is current VFS text, not an async FS read. |
| `codex-rs/apply-patch/src/file_update_tests.rs` | `src/file_update_tests.rs` | All five tests ported to synchronous calls; diff generation is test-only. |
| `codex-rs/apply-patch/src/lib.rs` | `src/lib.rs`, `src/error.rs`, `src/apply.rs`, `tests/` | Args/errors and application semantics adapted to the plan/commit API below; application tests use this API. |
| `codex-rs/apply-patch/tests/fixtures/scenarios/` | `tests/fixtures/scenarios/` | Scenarios 001–024 (25 directories, including two 020 scenarios); `.gitattributes` preserves CRLF/mixed bytes. |
| `codex-rs/core/assets/tools/apply_patch.lark` | `assets/apply_patch.lark` | Byte-identical; exposed as `tool_contract::LARK_GRAMMAR`. |
| `codex-rs/core/src/tools/handlers/apply_patch_spec.rs` | `src/tool_contract.rs` | Only the tool name and the freeform description string. |

Not ported: `invocation.rs` (shell and heredoc detection with tree-sitter),
`standalone_executable.rs`, `main.rs`, production unified-diff APIs, and the
executor-specific filesystem/delta APIs.

Added locally, with no upstream counterpart: `src/targets.rs`
(`targets`, `targets_raw`, `PatchTarget`, `TargetKind`, `TargetsError`),
`tool_contract::JSON_DESCRIPTION` / `tool_contract::json_schema` for the JSON
function variant of the tool, and the VFS planning/atomic commit API.

## Intentional differences

### API (S)

- S1. `std` paths replace `PathUri` and the `codex_exec_server` file system,
  and the API is synchronous. `Hunk::resolve_path(&self, cwd: &Path) ->
  PathBuf` joins the source path onto `cwd` (an absolute path replaces `cwd`)
  and cannot fail. `Hunk::path()` instead returns a move's destination.
  Upstream resolution takes `&PathUri` and returns
  `Result<PathUri, PathUriParseError>`.
- S2. `ApplyPatchArgs` has no `workdir` field. Upstream fills it only in
  `invocation.rs`, which is not ported.
- S3. `ApplyPatchError` has no `PathUri` or `ImplicitInvocation` variant;
  neither can occur without `PathUri` and `invocation.rs`. Local `Path` and
  `NoFilesModified` variants cover path validation and empty patches.
  `ApplyPatchError::class()` exposes `ErrorClass::{Parse, Match, Io, Path}`.
- S4. `ParseError` moved from `parser.rs` to `error.rs`, with the same variants
  and messages. `IoError` fields are `pub(crate)` instead of private so other
  modules can build it. `ApplyPatchError` formats parse failures with the
  capitalized upstream application messages.
- S5. `anyhow`, `pretty_assertions` and the `codex_utils_*` crates are not used.
  `tempfile` is used for atomic writes and test directories; `similar` is
  test-only. Upstream parser tests use paths under `std::env::temp_dir()`;
  none of them touch the filesystem. One test was added:
  `test_hunk_resolve_path_uses_update_source_and_path_uses_move_destination`.
- S6. `plan(patch, root)` returns a read-only `Plan`; `Plan::commit(self)`
  returns `Applied` or `CommitFailure`. `apply_patch(patch, root)` combines
  them and returns `ApplyError::{Plan, Commit}`. No stdout/stderr writers or
  executor FS arguments are required. Relative paths require an explicit
  root, and patch paths containing `..` are rejected via `targets`.
- S7. `Applied.changes` and `CommitFailure.committed` describe final disk
  mutations, aggregated to one entry per final resolved path. `ChangeKind`
  retains old/new text, overwritten text, and a completed move's original
  source. A move destination already equal in content and mode needs no
  write, so only its source deletion is reported. In contrast,
  `Applied.summary` lists each hunk in upstream A/M/D order with the raw
  patch spelling, including repeated paths and no-ops.

### Behavior (D)

Patch parsing behaves exactly as upstream, including lenient heredoc
stripping, whitespace-padded markers, error messages, and the optional
`*** Environment ID:` preamble (accepted and returned in
`ApplyPatchArgs::environment_id`, which evorch ignores; the grammar sent to
models does not mention it).

Differences in applying a patch:

- D1. A move whose source and destination resolve to the same path is treated
  as an ordinary update, not a write followed by deletion. The file remains
  present and is reported as `ChangeKind::Updated` when changed.
- D2. Commit is not a multi-file transaction and does not roll back. An I/O
  failure returns `CommitFailure { error, committed }`. Its display retains
  the upstream operation message and, only for a nonempty committed list,
  appends `\nApplied before the failure:\n` followed by A/M/D lines. A move
  is recorded as completed only after its original source is removed;
  otherwise its persisted destination is an add/update. Temporary files are
  cleaned up, but parent directories created before a failure may remain.
  Planning and commit do not lock files or detect external edits; callers
  must serialize competing writes.
- D3. Every hunk is validated against a lazy in-memory VFS before any write.
  Parse, path, match, or read failures leave the filesystem unchanged, even
  after earlier valid hunks. Sequential operations on the same resolved path
  observe prior virtual changes (updates, add→update, move→destination update,
  delete→add). Upstream applies hunks directly; scenario 015 therefore has an
  intentionally different expected outcome: the initial tree is preserved.
- D4. Commit writes final files in resolved-path order before any deletions,
  using same-directory `NamedTempFile::persist`. Existing permissions are
  preserved and moves inherit source permissions. Unchanged final states are
  not written; new files use tempfile's restrictive default permissions.
  This changes write ordering and inode/hard-link behavior versus in-place
  writes. Symlink reads follow std; atomic writes follow the final symlink
  rather than replacing it, and deletion uses `std::fs::remove_file`.
  Symlink authorization is the executor's responsibility.
- D5. The VFS records UTF-8 text before overwriting or deleting it. Unreadable
  or non-UTF-8 existing files are planning errors rather than permitting an
  inexact content delta. The executor-specific upstream `exact` flag has no
  local counterpart.

The matching implementation follows the pinned code, including EOF searches
restricted to the final position. Upstream's comment describes an earlier-line
fallback that its implementation does not perform; only that comment is
corrected here, not the matching behavior.
