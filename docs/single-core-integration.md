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
| CI | `scripts/contract-test-filters.sh` registers the cache target `core_settings_cache`. The filter is a central file: a new cache target is registered by the integration PR that precedes its feature PR (nextest rejects a name that matches no binary, so that PR also adds a placeholder file for it). `scripts/central-files.txt`, `scripts/feature-scopes.txt`, `scripts/check-central-files.sh` and `scripts/ci-scope-gate.sh` keep a feature PR out of the central files, and the `scope-gate` job of `ci` runs that check on every PR. `scripts/check-core-client-refs.sh` keeps `core_client` from naming internal crates beyond its baseline, in every form Rust accepts: `crates/core-client-refs` reads each file with `syn` (run through `scripts/check-core-client-refs.sh`): `use` trees (groups, `as` aliases, `::crate::`, raw identifiers), paths in expressions, types, patterns and attributes, the tokens of macro invocations and definitions, and lexical scopes (a module or block that defines `storage` hides the crate there only; `::storage` always means the crate; macro metavariables are not crate names). It is a leaf crate with no workspace dependency, in the `support` test group; it adds one package to `Cargo.lock` (its dependencies `syn`, `proc-macro2` and `tempfile` are already locked). Limits: the macro scan is conservative (a token sequence that looks like a path from a crate counts, except the arguments of `stringify!`, `concat!` and the other text macros); a module in a file that is not `.rs` (`#[path]`, `include!`) is not followed; and it targets references written by mistake, not ones hidden on purpose. |

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

## PR 10: settings and auth

The `settings` and `auth` namespaces are implemented in the core
(`areas/settings.rs`, `areas/auth.rs`), the adapters, the desktop features and
the Web API at once. Operations (`save_document`, `save_provider`,
`delete_provider`, `reload_models`, `save_api_key`, `start_login`, ...) are
accepted at once and settle through `settings_operation_completed` /
`auth_operation_completed`.

| Contract | Where it is held | Where it is tested |
|---|---|---|
| R1: a login wait holds no restart permit; a planned restart or shutdown ends it as `Cancelled(Restart \| Shutdown)`; the result is written only after entering the gate again | `areas/auth.rs`, `areas/auth/login.rs` | `workbench-host/tests/auth_login.rs` (a restart requested during the wait seals without the login, and the login ends settled) |
| A login's preparation (reading the profile, binding the callback port) runs in the tracked operation task, the handler only looks at memory, and the accepted permit is held until the login waits for a person | `areas/auth.rs`, `areas/auth/login.rs` | `auth_login.rs` (a held preparation does not block the handler and keeps the profile; a cancel during it ends the login) |
| Whatever way a login ends, its callback listener and flow are released before the outcome is written | `areas/auth/provider_logins.rs` (`LoginFlow::release`) | `auth_login.rs`, `provider_logins.rs` unit tests (real listeners are bound again after the release) |
| Cursor signs in with a dashboard API key through the provider's own exchange; the key is never stored | `areas/auth.rs` (`LoginStarter::exchange_api_key`) | `auth_login.rs` (`a_cursor_api_key_is_exchanged_and_stored_as_a_login`) |
| `applied_revision` is never newer than the file the reload hook composed from (the hook runs again, a few times at most, when the file moved while it ran, so the display converges without another reload), and a slow re-read never replaces a newer snapshot; every change is announced | `areas/settings.rs` | `settings_save.rs` (a save during a held reload; overlapping saves and reloads) |
| An operation the restarted core does not know is *unknown*, not running (a login still being prepared has no session yet: `not_found` for the session is answered by asking for the operation, and only an operation the core does not know either is lost): the screen is not busy for ever and the draft is kept; text typed while a save runs is not discarded when it ends | `gui/src/core_client/features/{settings,auth}.rs` | the feature tests of both |
| `core.restart.targets` cannot be added, removed, repointed or omitted by a settings save, whole or partial, judged inside the revision lock | `areas/settings/store.rs` on `config::reject_restart_target_changes` | `workbench-host/tests/settings_save.rs`; through the real desktop screen and the real Web HTTP API in `core-client-e2e/tests/settings_restart_targets.rs` |
| A save never rewrites a sent provider prefix; a reload replaces the model of the projects and the next request still extends what was sent | `bootstrap` reload hook (`SwitchableModel::replace`) | `evorch-core/tests/core_settings_cache.rs` (the replacement is observed at a second endpoint) |

### Threat model: remote control

With `[web] enabled` and Tailscale Serve (`docs/config/evorch-core.md`), whoever
passes Tailscale's authentication and ACL can issue every command of the
protocol, with the provenance of a human on the Web surface. Settings and auth
are the commands where that matters most, so:

- **Restart targets are not a setting a client can change.** A restart target is
  an executable the core starts on a planned restart. If a Web client could
  rewrite it, one settings save would be the start of an arbitrary program, so
  the core refuses every save that changes `core.restart.targets` (permission
  denied; the file is untouched), from the desktop and from the Web alike. The
  targets are changed only by editing the config file on the machine that runs
  the core. The check compares the targets of the file as it is (the whole text
  of a document save, the file after a partial save) and of the effective
  configuration (all layers), so a key moved to another layer or spelled in a
  form the parser reads differently does not pass; an omission counts as a
  removal. It runs under the same lock as the revision comparison and the write.
- **A save is checked as the core would load it.** `save_document` accepts only
  text the core's own strict loader (`Config::load_strict`, the load the core
  starts with: an unknown field or a wrong type is refused, not dropped) accepts.
  A typed save (`save_provider`, `delete_provider`) does not edit a file that
  loader already refuses, and puts back a result it refuses. A whole save keeps
  the mode of the file it replaces. The scratch copy the check loads is
  owner-only (0700 directory, 0600 files). The revision is compared in the
  same critical section as the write, so two clients saving one revision end as
  one success and one `stale_revision`.
- **Secrets are write-only.** An API key travels once inside `save_api_key` and
  goes straight to the credential store. It is not in any operation, event,
  receipt, log line or repeat-detection digest, and no query returns it; the
  snapshot says only whether a credential is present (and, for a login, when it
  expires). `SecretValue` redacts itself in `Debug`.
- **Logins.** The authorize URL is built by the core. A Codex or Claude login
  listens on a loopback port of the machine that runs the core, so a person on a
  phone finishes it by pasting the redirect address (or code) into the client;
  the pasted text is checked against the `state` the login started with and is
  accepted once, whichever of the listener and the paste arrives first. The
  listener is released when the login ends, however it ends. Cursor's API-key
  sign-in goes through `save_api_key` and the provider's exchange; the key is
  not stored, only the session the exchange returns. Neither
  the callback nor the pasted text is logged or stored. Only the owner of the
  Web session's network position can finish a login, so a login started over the
  Web is as trusted as Tailscale's boundary and no more.
- **Not a boundary:** `allowed_hosts` / `allowed_origins` stop a browser page
  from another origin; they do not tell two Tailscale users apart. Do not expose
  the Web port through a public reverse proxy.

## Who edits what after S0

See `scripts/feature-scopes.txt` (the machine-readable form) and
`scripts/central-files.txt`. A feature PR changes only the files of its own
areas: `workbench-protocol/src/namespaces/<area>.rs` (or `<area>/`), the
adapter `ingress/namespaces/<area>.rs`, the service `areas/<area>.rs`, the GUI
feature `features/<area>.rs`, and the area's samples and golden file. The
cache scenarios it adds go into the target S0 registered.

## The scope gate

The `scope-gate` job of `ci` (a need of the aggregate `ci` job) runs
`scripts/ci-scope-gate.sh` on every pull request, with the full history so that
the merge base with the target branch exists. The scope is the head branch name:

- `<type>/pr<N>[-slug]` (`feat/pr10-settings`, `feat/pr17e-offline`) is a feature
  PR. Every changed file must be outside `scripts/central-files.txt` and inside
  that scope in `scripts/feature-scopes.txt`.
- `<type>/s<N>[-slug]` and `<type>/integration[-slug]` are integration PRs. They may
  change central files; the check only lists them.
- Any other branch is not part of the migration and is not checked. The gate
  is therefore opt-in by naming, and the Orchestrator names every migration
  branch. The name is fixed when the PR is opened, so editing the PR description
  does not rerun CI.

A diff that cannot be computed (an unknown base) fails the gate; it never counts
as "no changes". To run it locally: `scripts/check-central-files.sh --scope pr10
--base origin/main`.

A change to a central file goes through the Orchestrator into the next
integration PR.

PR 16a-1 (#610) is the one exception the Orchestrator approved: it adds two
lines to `scripts/ci-test-groups.json` (a central file) for its renderer crates,
which are not in the `pr16a1` scope either. Its branch `feat/renderer-helper`
carries no `pr<N>` scope, so the gate does not check it. A rename to
`pr16a1-...` would make the gate refuse it, which is intended: the renderer
crates enter through the small integration PR that R2 asks for once the
measurement is accepted.

## Reserved migration numbers

Storage migrations are numbered from one shared ledger kept by the
Orchestrator: v62 (07b-1, #602) and v63 (07b-2, #603) are merged; v64 is taken
by 07b-2b (learning attempts). S0 adds none and reserves none. A PR that needs a
migration asks the ledger for the next free number (v65 at the time of writing),
and at most one numbered migration PR waits to merge at a time.
