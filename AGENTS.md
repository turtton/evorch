# AGENTS.md — working policy

This repository (`turtton/evorch`) is both the **intent host** for the
`evorch` domain and its **implementation repository**. It owns the durable
intent host state under `.intent-cli/` and `intents/evorch/` and also contains
the evorch code (`crates/` etc.).

- Target repo (implementation): `turtton/evorch` (same repository as the host)
- Domain: `evorch`
- Bootstrapped by `intent-cli intent init` (G293 / G301).

## Working policy

- **Host state and operating docs** — `.intent-cli/` (queue-state, runs.jsonl,
  packets), `intents/`, `docs/`, `AGENTS.md`/`CLAUDE.md`: work directly on
  `main` and do NOT open a PR unless the operator explicitly asks for one.
- **evorch code changes** (`crates/` etc.): work in a worktree and open a PR
  (worktree-pr skill). The "directly on `main`" rule above does not apply.
- Real commits must use an email verified on the committer's GitHub account
  and must be signed with the configured signing method and key. Signing
  failures must not be bypassed. GitHub must report the pushed commits as
  verified. The `git-best-practices` skill documents the general identity
  and signature checks; these requirements also apply when that skill is
  unavailable.
- `git pull --ff-only` before edits. Commit and push after each coherent
  change (to `main` for host state / operating docs, to the PR branch for
  code).
- All workflow label transitions go through installed `intent-cli automation`
  / `intent-cli worker` commands. Never edit GitHub labels by hand.
- Routine collaboration uses `intent-cli guide ...`. Do NOT read
  `intents/rules/**`, copied prompt files, or local skill files that restate
  workflow for routine operation.
- CARVE-OUT: the CLI-owned `intent-cli` dispatcher skill installed by `intent-cli skill install` is PERMITTED — it restates no workflow, is single-sourced from this CLI with `intent-cli skill diff` drift detection, and is distributed only by `intent-cli skill install`. Local skill files that restate workflow (`gh-issue-to-pr`, `gh-fix-pr-comment`, copied runbooks) remain forbidden.
- Do NOT call `intent-cli run` (advanced runtime) or `dotnet run` as a
  fallback. Do NOT ask `intent-cli` to launch Claude/Codex.

## intent-cli knowledge recovery (post-compression)

- `intent-cli` is **self-describing**: never answer intent-cli command /
  concept questions from conversation memory alone. Resolve them from the CLI
  itself first — `intent-cli --help`, `intent-cli guide commands list`,
  `intent-cli <group> --help` (guide / worker / automation / packet / issue /
  closeout / interview), and per-topic guides such as `intent-cli grill`,
  `intent-cli inspect`, `intent-cli stack`, `intent-cli improve`,
  `intent-cli next`.
- Interview / guide protocols (`grill`, `inspect`, `stack`, `improve`, `next`)
  are durable G-numbered guides owned by the CLI, not by this repo. The CLI is
  the single source of truth; a stale recalled summary is a bug, re-fetching
  from the CLI is the fix.
- When operating `intent-cli`, always read `docs/intent-cli-ledger.md` first.
  It is the append-only ledger of verified interfaces and known drifts that
  `--help` and the guides do not reveal.
- When compressing long sessions, keep only the pointers to the CLI and to
  `docs/intent-cli-ledger.md`; do NOT copy command catalogs or ledger entries
  into summaries (they go stale).

## Compatibility policy (pre-v1)

While evorch is v0.x, do NOT preserve backward compatibility of internal
on-disk formats. Persisted layouts / panel registries / event or storage
schemas may change freely (e.g. new `PanelKind` variants) without downgrade
tolerance or migration layers. Old persistence may simply fail to load on a
mismatch. Revisit this policy before cutting a v1 release.

## Development environment

- The default Nix dev shell resolves `cargo` to the huacnlee/cargo fork (a
  trial, pinned in `flake.nix`, with `CARGO_FORK_AUTO_UPDATE=0`). rustc,
  rustfmt, clippy and rust-analyzer still come from the fenix toolchain. CI
  and the crane builds keep the upstream cargo. `nix develop .#upstream-cargo`
  gives the previous setup: fenix's cargo, a `target/` per checkout, sccache.
- The Cargo selected inside a sandbox depends on the evorch process's
  `PATH`; entering a dev shell in another terminal does not select the fork
  for an already-running evorch. To share the fork's build pool across thread
  scratches, explicitly allow the existing host `~/.cargo/targets`
  (read-write for Worker and Reviewer, read-only for other roles).
- Allowed canonical strict descendants of the process HOME are also bound at
  the same relative paths below `/tmp/home`, with the same access as their
  original absolute binds. HOME is canonicalized at boundary construction;
  invalid, non-directory or root HOME disables mirrors. HOME itself, parents,
  siblings and external paths never mirror. Explicit permissions still apply
  when automatic cache sharing is disabled. Allow only targets for build
  sharing: allowing all of `~/.cargo` also exposes config and credentials.
- The sandbox does not inherit `RUSTC_WRAPPER`, `CARGO_FORK_AUTO_UPDATE` or
  sccache settings. When needed, run
  `CARGO_FORK_AUTO_UPDATE=0 RUSTC_WRAPPER= cargo ...` explicitly; sccache
  configuration and server access are not implicitly shared either.
- `gui` enables CEF by default, so cargo builds of it need the CEF distribution.
  The default Nix dev shell sets `CEF_PATH` and `LD_LIBRARY_PATH` for it, but a
  sandbox does not inherit them: pass both explicitly (docs/browser.md,
  `scripts/setup-cef.sh`). The `sandbox-build` skill gives a wrapper that
  loads the whole dev shell environment (also `LIBCLANG_PATH`) in a sandbox.
- The fork ignores `CARGO_TARGET_DIR`, `build.target-dir` and
  `build.build-dir` (with a warning), so do not set them. Intermediate
  artifacts go to `${CARGO_HOME:-~/.cargo}/targets/<workspace>-<hash>/`, with
  dependencies in `shared-deps/`, shared by every checkout; only final and
  test binaries land in each checkout's `target/`. A new checkout of the same
  commit reuses the workspace crates as well, keyed by file content, with two
  exceptions that are rebuilt in every new worktree:
  - A unit whose rustc dep-info names `CARGO_MANIFEST_DIR` or
    `CARGO_BIN_EXE_*`. In tests, read them at run time with
    `std::env::var("CARGO_MANIFEST_DIR")` (`cargo test` and `cargo nextest`
    set both) instead of `env!`.
  - A file the unit reads that is not content-keyed, which falls back to an
    mtime check that a fresh worktree always fails. The fork keys a target by
    the files of its package in directories that hold no other target's root.
    The root files of other targets (`tests/*.rs`), the directories they sit
    in (`tests/`, `examples/`, `src/` for an integration test) and anything
    outside the package (`docs/`, other crates) are mtime-checked. Put data
    that a lib and its tests both `include_str!` in a directory other than
    `src/` and `tests/` (for example `fixtures/`, `undo/`,
    `shared-test-support/`), and read files outside the package at run time
    rather than with `include_str!`; otherwise a lib reading from `tests/`
    invalidates its crate and every dependent.
- Those directories live outside the repository and are not removed with a
  worktree; delete them by hand (`rm -rf ~/.cargo/targets/<dir>`) when they
  grow large.
- The fork shell does not use sccache: an unset `RUSTC_WRAPPER` becomes the
  empty string, which also overrides a `build.rustc-wrapper` in Cargo config
  (the user's `~/.cargo/config.toml` may set one). A non-empty `RUSTC_WRAPPER`
  that is already set when the shell starts is kept.
- The rest of this section describes the `upstream-cargo` shell. Every
  checkout (the main clone and each git worktree) builds into its own
  `target/`, Cargo's default (gitignored). Do not point `CARGO_TARGET_DIR` at
  one directory from several checkouts: their same-named workspace crates
  overwrite each other's artifacts there and cause spurious compile errors.
- That shell sets `RUSTC_WRAPPER=sccache`, so most compiled dependency
  crates are reused across checkouts. sccache does not cache workspace crates
  (incremental in the dev profile), proc-macros or build scripts, and
  dependencies that compile checkout-specific paths, such as build-script
  output in `OUT_DIR`, miss once in each checkout.
- The cache uses sccache's defaults: `~/.cache/sccache` on Linux
  (`~/Library/Caches/Mozilla.sccache` on macOS), capped at 10 GiB.
  `SCCACHE_DIR` and `SCCACHE_CACHE_SIZE` (for example `20G`) change them, but
  a running sccache server keeps the values it started with. One server is
  shared by every checkout and by host builds that use sccache. To apply new
  values, run `sccache --stop-server`; the next compile from any build starts
  a new server with that build's environment. Stopping the server also resets
  the statistics of every build sharing it, and `sccache --show-stats` counts
  all of those builds, not only this checkout's.
- In the `upstream-cargo` shell, a `RUSTC_WRAPPER` that is already set when
  the shell starts is kept. To disable sccache, set it to the empty string:
  `RUSTC_WRAPPER= nix develop .#upstream-cargo`, or
  `RUSTC_WRAPPER= cargo build` for one command. The empty value also
  overrides a `build.rustc-wrapper` in Cargo config.

## Wrong-host detection (G301)

`.intent-cli/host-binding.toml` records the canonical host repo for this
domain. If you operate this domain from a different host repo, expect
`intent-cli` to surface a structured wrong-host warning with remediation
steps; do not silently proceed with parent-state mutation.

## Cache correctness gate

Prompt cache preservation is a cost-sensitive correctness contract. CI enforces
this contract with `scripts/check-cache-contracts.sh`. Run the script manually
when focused local cache validation is needed.

- Normal turns must preserve the already-sent provider input prefix and stable
  instructions, model/settings, tool schemas/order, and cache affinity key.
- Bound tool output and create artifact references when returning the result.
  Do not rewrite previously sent tool results based on age or result count.
- A successful compaction is an explicit one-request history-replacement
  boundary. Its system/tools remain stable, and later turns must preserve the
  new prefix. A checkpoint-looking string alone is not permission to reset.
- A role-chain model switch is also an explicit boundary: the cache restarts on
  the new member, the re-summary or compaction prepared for it is that
  boundary's one history replacement, and later turns must preserve the new
  member's prefix.
- Cache-affecting features must extend the end-to-end scenarios for their
  affected paths. Keep the input-derived mock cache and independent wire-prefix
  assertions; fixed high cached-token fixtures alone cannot prove preservation.
- Do not weaken or remove this contract to make a new feature pass. Intentional
  invalidation requires a documented boundary and tests for reuse after it.

## Current role boundary (ADR 0035, ADR 0040)

- Advisor (formerly Oracle) is an optional, read-only consultant before a decision,
  not a mandatory stage. Return options, trade-offs, a recommendation, uncertainty,
  and information that would change the decision.
- The Orchestrator consults Advisor when it needs a step back: a must finding
  persists after re-review or Worker and Reviewer disagree; a child's technical
  question is not settled by the requirements and the Orchestrator lacks grounds
  to decide it alone; or collected evidence does not narrow the cause hypotheses
  (Advisor proposes the checks that discriminate them; it runs nothing).
  Permission and user-intent questions go to the user, not Advisor.
- Implementation belongs to Worker; detailed execution plans belong to Planner.
  Comparing implementation results or plans against requirements and issuing an
  acceptance verdict belongs exclusively to Reviewer.
- The final independent Reviewer must use a separate context from the consulted
  Advisor. Advice is not an acceptance verdict.
