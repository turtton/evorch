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

## intent-cli known drifts & verified interfaces (ledger)

Hard-won operational findings that compression must NOT lose. Each entry is
pinned to the verified binary version + date. Entries are append-only;
remove an entry only after verifying against a newer binary.

- **Dual-check rule** (verified 0.26.0, 2026-08-30): for any unfamiliar
  intent-cli workflow, consult BOTH `intent-cli <cmd> --help` (what the
  binary implements) AND `intent-cli guide <topic>` / `intent-cli grill`
  (the documented protocol). A contradiction between them is a real finding:
  the binary wins for execution, and the drift is a bug-report candidate.
  Neither source alone reveals the contradiction.
- **interview record-answer** (verified 0.26.0, 2026-08-30): the binary
  rejects the guide-documented `--question-id <id> --answer "..."` form.
  Implemented form: `record-answer --session <id> --question <q> --from-file
  <path> --write` (new questions additionally need `--prompt <text>`;
  re-answering an existing id does not). `next-question` requires
  `--session` (the guide-documented `--domain`-only form errors).
  `interview answer --domain <d> [--from-file <path>]` answers the next
  pending question.
- **grill protocol** (verified 0.26.0, 2026-08-30): `intent-cli grill` is
  persistent interview mode — the agent owns semantic questioning; one
  focused question per turn (never batch); dependency-ordered backlog;
  record every answer durably before proceeding; stop only at a structured
  stop condition (backlog empty + rediscovery finds nothing →
  `今のところ追加質問はありません`).
- **stack: queue seed path** (verified 0.26.0, 2026-08-30): `queue enqueue`
  is NOT the entry for new packets — it errors ("Projection packet YAML must
  contain root field 'execution_unit'") because it expects an existing
  `publish.yaml`. Correct seed: `automation queue-seed-from-packet
  --execution-unit <id> --domain <d> --target-repo <r> --write` (refuses
  without `--target-repo`; default priority `high`). Reprioritize needs
  `--reason` AND `--write` (run without `--write` for a preview). The CLI
  leaves an empty `.intent-cli/queue-state.reprioritize.lock` behind — do
  not commit it.
- **stack: issue publish flow** (verified 0.26.0, 2026-08-30): G337 four
  stages — `issue draft <unit>` (read-only) → `issue create <unit>` →
  `issue publish-flow <unit> --repo <r> --domain <d> --write` (advances
  publish.yaml + queue-state + runs.jsonl; run WITHOUT `--write` first as
  the canonical preflight) → `automation issue-publish --repo <r> --issue
  <n> --write` (the ONLY `intent-target` applier). Known quirk: publish-flow
  ignores `issue_title` in packet.yaml and falls back to `<unit>
  (untitled)` (v0.1 identical) — correct the title with `gh issue edit`
  after publish-flow. `publish-recovery` without `--domain` reports
  domain-underivable unsafe_stops; with `--domain`, queued-but-unpublished
  units report missing-publish-artifact which is normal pre-publish state,
  not a blocker. WIP cap G288: one `intent-target` issue/PR per domain.
  Claims store is not configured (`claim verify` → not-configured), so no
  G717 handoff is needed in this environment.

## Wrong-host detection (G301)

`.intent-cli/host-binding.toml` records the canonical host repo for this
domain. If you operate this domain from a different host repo, expect
`intent-cli` to surface a structured wrong-host warning with remediation
steps; do not silently proceed with parent-state mutation.