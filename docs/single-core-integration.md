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
| Cursor signs in with a dashboard API key through the provider's own exchange; the typed key is kept inside the stored login bundle (the provider re-exchanges it to renew the short-lived access token), in the safe credential store under the profile's account, and no query, event or operation result ever returns it | `areas/auth.rs` (`LoginStarter::exchange_api_key`) | `auth_login.rs` (`a_cursor_api_key_is_exchanged_and_stored_as_a_login`) |
| `applied_revision` is exactly the file the reload hook composed from: a reload and the saves are serialized by one lock from the read of the revision to the end of the hook, so the hook runs once per reload and a save accepted meanwhile waits (its operation stays pending; queries and other areas go on). A revision is a content hash, so only this serialization (not a compare after the hook) tells A, B, A apart. If the hook fails it may already have replaced some of the models (the startup one, then a project that did not load), so no file is known to be what the runs use: `applied_revision` becomes `None` with the failure in `apply_failure`, and stays so, whatever is saved next (a file put back has the hash that was applied before, and is not shown as applied), until a reload succeeds; the GUI says the state is unknown and why. A slow re-read never replaces a newer snapshot; every change is announced | `areas/settings.rs` | `settings_save.rs` (a save accepted during a held reload waits and the hook composed what was marked applied; a failed reload leaves the applied state unknown until one succeeds; a stalled reload; overlapping saves and reloads), `gui/.../settings/tests.rs` (an unknown state is shown with its reason, not as applied), `evorch-core/tests/core_settings_cache.rs` (the replaced model is observed at a second endpoint and the next request still extends the sent prefix) |
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
  sign-in goes through `save_api_key` and the provider's exchange. The typed
  key is *kept*: it is stored inside the login bundle the exchange produces
  (`CursorTokenBundle.api_key`), because the provider renews the short-lived
  access token by exchanging the key again. The bundle lives in the safe
  credential store under the profile's account (the same place as any other
  login) and is never sent to a client: the key is not in any status, event or
  operation result, and a refused exchange stores nothing. Neither
  the callback nor the pasted text is logged or stored. Only the owner of the
  Web session's network position can finish a login, so a login started over the
  Web is as trusted as Tailscale's boundary and no more.
- **Not a boundary:** `allowed_hosts` / `allowed_origins` stop a browser page
  from another origin; they do not tell two Tailscale users apart. Do not expose
  the Web port through a public reverse proxy.

### Known constraint: the Evaluator's reload path

The Evaluator's reload (`bootstrap/mod.rs`, the `evaluator.reload` closure)
calls the same hook directly. It does not go through the settings area's writer
lock or its snapshot, so a reload it starts is not serialized with a save and
does not move `applied_revision` (or mark it unknown when it fails). The
settings area's own reload, which is the one a client can start, is
unaffected. Routing the Evaluator through the area is outside PR 10's scope
(`bootstrap/mod.rs` is a central file); it is handed to the integration PR or
17c.

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

## I-14: the conditional update of the management screens

Stage 14 moves the update operations of `memory` and `improvement` into the
core. I-14 is the integration PR that lands what they share. It publishes no
operation, command or capability.

| Contract | Where it is held | Where it is tested |
|---|---|---|
| A conditional update reads the entity, compares the revision, judges the action and writes in one `IMMEDIATE` transaction on the existing single writer (`StorageHandle::update_memory` / `update_improvement`, module `storage::management`). The host never reads first and writes afterwards; there is no second writer and no GUI write to the database. | `crates/storage/src/management.rs`, `writer.rs` (`Command::UpdateMemory` / `UpdateImprovement`) | `crates/storage/tests/management_cas.rs` (barrier races, no sleeps) |
| Three refusals are distinct and judged in this order: `NotFound` (the entity is gone or never existed), `StaleRevision { current }`, `Denied(Denial)` (the state forbids the action, for example rejecting a favorite lesson). A refusal writes nothing, so the revision does not move, and the writer keeps serving. | `CasRefusal` | `the_three_refusals_are_distinct`, `a_failed_write_rolls_back_and_the_writer_keeps_working` |
| `Applied { revision }` carries the revision after the write; `Unchanged { revision }` is a successful no-op (attaching the draft already attached). Learning, validation, promotion and intake writes go through the same writer, so an update that names a revision from before one of them is stale instead of overwriting it. | `CasOutcome` | `a_learning_write_*`, `a_folded_duplicate_*`, `an_update_that_changes_nothing_keeps_the_revision` |
| The read side and the update share one definition. The lesson revision is `Database::memory_revision` (the count of status rows and favorite changes of the ledger), the candidate revision is `improvement_revision` (the first eight bytes of SHA-256 over the JSON of status, occurrences, last seen, reopen count, reopen time and draft path), and the shown `actions` are `memory_denial` / `improvement_denial`. The stage 13 wire DTOs and values are unchanged. | `areas/memory.rs`, `areas/improvement.rs` call the storage functions | `crates/workbench-host/tests/management_cas_parity.rs` (the shown revision and actions equal what the update accepts, in every state; the improvement digest is pinned) |

Notes for stage 14:

- `expected_revision` of a command is the wire `Revision` the read side showed.
  Map `CasRefusal` to the stale, not-found and denied errors, and `Denial` to
  `DenialReason` with `areas::memory::denial_reason`. `Unchanged` is a success.
- Triage writes nothing but `AttachDraft`. The host makes the draft from safe
  references and the core never sends it on its own. A path that is not a valid
  draft reference is a storage error, not a refusal.
- `pr14` owns `crates/storage/src/management.rs`, `management/*` and
  `crates/*/tests/management_cas*`; `writer.rs` stays central. A needed new
  operation goes through the next integration PR.
- I-14 also registers the cache target `core_pr_cache` of PR 15 (a placeholder
  file, in `CACHE_CONTRACT_FILTER`). `scripts/tests/test_check_central_files.sh`
  checks that every binary named in the cache filter has a test target.

## PR 15 (part A): pull requests in the core

Stage 15 is **not complete**. This part moves the pull request operations of
the `pull_requests` namespace into the core (protocol, host area and ingress
adapter, the desktop `core_client` feature, `core_pr_cache`). The GUI screens
still reach the pull request service and the database directly; switching them
is part B (below).

The pull request service of the runtime stays the only writer and the only place that decides. The area
(`workbench-host/src/areas/pull_requests.rs`) only translates: it never runs
`gh`, never writes the database and never decides whether an automatic notice
may be sent.

| Contract | Where it is held | Where it is tested |
|---|---|---|
| Link, unlink, refresh, candidate confirm/dismiss, auto-fix policy, budget reset, destination change and the manual notice are commands answered at once with a pending operation; the work runs as a counted area task that keeps the restart permit it was accepted with until the outcome is settled. A handler never waits | `areas/pull_requests.rs`, `ingress/namespaces/pull_requests.rs` | `workbench-protocol/tests` (golden, contract), `evorch-core/tests/pull_requests_core.rs` |
| Races and resends: a link or unlink quotes the lifecycle revision, a policy change quotes the link and the policy revision, and the service compares them inside its writer, so two clients that read the same revision end as one success and one `stale_revision`. The same operation id with the same request joins the operation (applied once); the same id with another request is a `conflict`. The operation table keeps 64 outcomes for reading, and the area keeps the id and request digest of up to 8192 accepted commands beside it and never evicts one: a request sent again after its operation left the table is answered as an unknown outcome (`outcome_unknown`, never run a second time), and its id still conflicts with another request. When that memory is full a new id is refused with `limit_exceeded` before it is accepted, until the core restarts; ids it already knows are still answered. A refresh sent again while a fetch runs joins one follow-up | the service's writer; `areas/settings/operations.rs` (shared operation table), `areas/pull_requests.rs` (`Seen`) | `pull_requests_core.rs` (two clients racing to link and unlink; a command sent again, also after its operation was evicted; a refresh joining one follow-up) |
| An outcome that is not known is never a success: a failure after the effect may have been written, and an operation cut off by a panic or by the host stopping, settle as `outcome_unknown`. A client whose reply was lost asks for the operation; when the restarted core no longer knows it, the screen shows "unknown" and reads the thread again | `areas/pull_requests.rs` (`Failure::unknown`, the task guard), `core_client/features/pull_requests.rs` | `core_client/features/pull_requests/tests.rs` (a lost reply is checked by operation id and never shown as done; a stale command reads the thread again) |
| A fetch cut off by the core stopping is never committed: the answer that arrives after the drain has nowhere to go, and after a restart the link is still there with no facts from the cut-off fetch. The existing drain contract is unchanged | the service's fetch commit, the area's counted operation tasks | `pull_requests_core.rs` (`a_fetch_cut_off_by_the_core_stopping_is_never_committed`) |
| The auto-fix budget and the one notification of a limit stop are decided in the core only. The service (`PullRequestService::take_limit_notification`) reads the stop, checks the destination and the link revision and claims the notification through the storage writer's compare-and-set; the area only publishes the event for the caller that got the claim, so several clients (and several cores on one database) tell the person once, and none starts a second automatic fix | `runtime/src/pull_requests/service/coordinator.rs` (`take_limit_notification`), `storage::pull_request_auto_fix`; the area only translates | `coordinator/tests.rs` (`a_limit_stop_is_claimed_once_however_many_callers_ask`), `pull_requests_core.rs` (`the_limit_stop_is_announced_once_for_every_client_together`) |
| Events name the thread and the pull request and never carry the facts; the client reads the thread again | `translate` in the area | the protocol golden files |
| Cache: the commands and one automatic fix notice on the production wire keep the sent prefix and the affinity key (input-derived mock cache, independent wire assertions); the GUI path is covered by `pull_requests_cache` | `evorch-core/tests/core_pr_cache.rs` | the same, in `CACHE_CONTRACT_FILTER` |

### Hand-over to part B (done in part B as far as the scope allows)

The old GUI keeps two direct paths: `gui/src/runtime_sink/pull_requests.rs`
calls `PullRequestService` directly and `gui/src/app/pull_requests.rs`
(`StoragePullRequestReader`, the limit-notice claim) reads and claims in the
database directly. The scope of `pr15` does not cover these files, and ADR 0045
§12 removes them with the old GUI (stage 20, boundary allowlist zeroed in 19b),
not by rewriting it. The boundary allowlists hold no entry specific to pull
requests, so nothing is dropped from them here.

## PR 15 (part B): the `core_client` pull request screen

Part B closes what the screen needed before 19a can make the core client the
default:

- A `limit_exceeded` refusal is shown by its advice. `RetryAdvice::Never` (the
  area's memory of accepted commands is full) says only a restart of the core
  clears it; `UserActionRequired` (every kept operation is still running) says
  to wait for them. Neither is sent again. `FeatureError` carries the server's
  `retry` for this (`connection.rs` sets it; additive, other constructors
  leave it `None`).
- The reason an automatic fix stopped (other than the spent budget) is shown.
- The restart test in `pull_requests_core.rs` says what it checks: the restart
  gate driven directly, not the coordinator. The coordinator's order (producers
  drain, Sealing, host gate drain) is covered by the M2 drain tests.

Gaps of the `core_client` screen against the old GUI, to close before 19a:
sidebar badge and CI glyph; inline candidate chips by tool call; the harness
card; notifications pane and OS notifications; CI summary with a check list;
mismatch warnings (local checkout against the pull request); stale and
cooldown status lines (the cooldown time is read but not shown); hover reasons
on disabled controls; fork/rewind link inheritance display; a dialog for
linking (the screen has an inline field). Link, unlink, refresh, candidate
confirm/dismiss, the auto-fix toggle, budget reset, destination change with
confirmation, the manual notice and the limit notice from the event are present.

## Reserved migration numbers

Storage migrations are numbered from one shared ledger kept by the
Orchestrator, but the ledger below is old: the number to use is the highest one
in `crates/storage/src/migrations` plus one (v65 is used by #623; the next free
number at I-14 is v66). v62 (07b-1, #602) and v63 (07b-2, #603) are merged; v64
was taken by 07b-2b (learning attempts). S0 and I-14 add none. A PR that needs a
migration takes the next free number from the implementation, and at most one
numbered migration PR waits to merge at a time.
