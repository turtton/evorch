# AGENTS.md — host repo working policy

This is an **intent host repository**. It owns durable parent state for the
`evorch` domain under `.intent-cli/` and `intents/evorch/`.
Child implementation repos do NOT own this state.

- Target repo (child implementation): `turtton/evorch`
- Domain: `evorch`
- Bootstrapped by `intent-cli intent init` (G293 / G301).

## Host repo working policy

- Work directly on `main`. Do NOT open a PR for routine host-state updates
  (queue-state, runs.jsonl, packets, intents/) unless the operator explicitly
  asks for one.
- `git pull --ff-only` before edits. Commit and push to `main` after each
  coherent change.
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
- When compressing long sessions, keep at most this one pointer in the
  summary; do NOT copy command catalogs into summaries (they go stale).

## Wrong-host detection (G301)

`.intent-cli/host-binding.toml` records the canonical host repo for this
domain. If you operate this domain from a different host repo, expect
`intent-cli` to surface a structured wrong-host warning with remediation
steps; do not silently proceed with parent-state mutation.