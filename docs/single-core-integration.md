# Single-core migration: integration PRs and ownership (ADR 0045)

Feature PRs of the migration never edit the shared registration. An
*integration PR* ("S0", "S1", ...) lands it first; feature PRs branch from the
`origin/main` that contains it. This page records what S0 fixed.

## What S0 fixed

| Area | Contract |
|---|---|
| Protocol | The ten namespaces `settings`, `auth`, `diagnostics`, `memory`, `improvement`, `evaluation`, `pull_requests`, `maintenance`, `artifacts`, `browser` stay reserved; each has an empty samples slot (`tests/common/areas/<area>.rs`) and an empty golden file (`tests/golden/areas/<area>.json`). No capability is added. |
| Shared DTOs | `workbench_protocol::operation` (operation id, state, view) and `workbench_protocol::management` (memory, improvement and evaluation identity, status, actions, denial reasons, `Present/Missing/Problem`). They are not operations of any namespace yet. |
| Host | `Areas` has a placeholder service per namespace (`areas/<area>.rs`). `AreaHandles` also carries `credentials`, `reload_models`, `evaluations`, `evaluator` and `database`. Every command adapter takes the acceptance `HostCommandPermit`. Restart classification of an area's commands lives in `<area>_command_access` in its adapter. |
| GUI | `core_client/features` has a placeholder feature per namespace; none is available, none draws or sends. |
| CLI | `main.rs` only starts logging and calls `evorch::cli::dispatch`; each command has a module in `cli/` (`run`, `eval`, `reindex`, `backfill`, `maintenance`, `inspect`, `benchmark`). The online `backfill-events` has its own parser (`evorch::backfill`). Arguments and exit codes are unchanged. |
| CI | `scripts/contract-test-filters.sh` registers the cache target `core_settings_cache` (a later feature PR registers its own new cache target in the same change that adds the file; nextest rejects a name that matches no binary). `scripts/central-files.txt` and `scripts/feature-scopes.txt` plus `scripts/check-central-files.sh` check that a feature PR touches no central file. `scripts/check-core-client-refs.sh` keeps `core_client` from naming internal crates beyond its baseline. |

## Operations and the restart permit

A handler never waits. Work that waits or blocks becomes an operation that keeps
the permit it was accepted with until its outcome is settled. **Waiting for a
person (an authentication callback, a confirmation) holds no permit**: a planned
restart or a shutdown ends that wait and the operation settles as `Cancelled`
(`CancelReason::Restart` / `Shutdown`); when the wait ends by itself, the
operation enters the restart gate again at the point where it writes its result.
PR 10 must show this: a restart requested during a login wait seals without
waiting for the login, and the login ends in a settled state. The full contract
is the module documentation of `workbench_protocol::operation`.

## Who edits what after S0

See `scripts/feature-scopes.txt` (the machine-readable form) and
`scripts/central-files.txt`. A feature PR changes only the files of its own
areas: `workbench-protocol/src/namespaces/<area>.rs` (or `<area>/`), the
adapter `ingress/namespaces/<area>.rs`, the service `areas/<area>.rs`, the GUI
feature `features/<area>.rs`, and the area's samples and golden file. The
cache scenarios it adds go into the target S0 registered.

Run `scripts/check-central-files.sh --scope <pr>` before opening a feature PR.
A change to a central file goes through the Orchestrator into the next
integration PR.

## Reserved migration numbers

Storage migrations are numbered from one shared ledger: v62 (07b-1, #602) and
v63 (07b-2, #603). S0 adds none. The next free number is v64; S0 only reserves
it for the next integration or feature PR that proves it needs one.
