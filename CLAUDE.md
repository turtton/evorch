# CLAUDE.md — host repo guide for Claude / chat-first agents

This is an **intent host repository** for the `evorch` domain.
See `AGENTS.md` for the canonical host working policy. This file mirrors
that policy for Claude-specific tool conventions.

- Target repo: `turtton/evorch`

## Reading order

1. The current GitHub Issue body (when running an automation loop).
2. `AGENTS.md` (host policy baseline).
3. `intent-cli guide ...` (chat-first canonical guidance — never read
   `intents/rules/**` or workflow-restating local skills for routine
   operation; intent-cli installed by
   `intent-cli skill install` is exempt).

## Hard rules (host repo)

- Work directly on `main` for host-state updates; pull before edit, commit
  and push after each coherent change. Open a PR only if the operator
  explicitly asks.
- Workflow label transitions go through `intent-cli automation` /
  `intent-cli worker`. Never raw `gh ... edit --add-label`.
- Do NOT call `intent-cli run`, `dotnet run`, or ask intent-cli to launch
  an AI provider.
- On any `Could not find .intent-cli` failure, follow the structured
  fail-closed guidance (G299) — do not fall back to ordinary GitHub
  review or raw PR comments.