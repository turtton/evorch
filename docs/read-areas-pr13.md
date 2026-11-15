# Read areas of the single core (PR 13)

`diagnostics`, `memory`, `improvement` and `evaluation` are served by the core
as **read-only** namespaces. Their update side (memory and improvement actions,
evaluation jobs) belongs to PR 14 and PR 17c and is not part of this PR: the
namespaces name no command, so each advertises only `Capability::Queries` and no
new capability exists.

## Read operations

A read opens a database or a file, and a query handler runs under the host's
mutex and must not wait. So every read is an *operation* (ADR 0045):

1. The client names it with an `OperationId` and asks. The handler registers the
   operation and answers at once with its `OperationView` (`Pending`); the
   read runs on a blocking worker of an `AreaTasks` task. It opens the event
   database read-only (`Database::open_read_only`), never through the runtime's
   shared connection and never under the mutex.
2. When the operation settles, the area publishes `<area>_operation_changed`.
   The client asks the same request again under the same id; the core answers
   from the operation it kept, now `Ready` with the data (`ReadOutcome<T>`), or
   `Failed { code }` / `Cancelled { reason }`.
3. The same id with the same request joins; the same id with another request is a
   `Conflict`. `page` is not part of a request's identity: the next page of the
   same operation names the same id with the cursor the previous page returned.
4. The table keeps 64 operations per area. A full table of running operations
   refuses (`LimitExceeded`) before accepting; otherwise the oldest settled one is
   dropped. Nothing is persisted: after a restart the table is empty and the
   client asks again.
5. The task is a counted one entered through the host's producer gate. A planned
   restart waits for a running read and a paused gate holds a read back. A read that
   never started, or whose task ended without a result, settles as
   `Cancelled(Shutdown)` (a guard created when the task is spawned), so an
   accepted operation is never lost. A read holds no restart permit while it
   waits for anything a person does, because it never waits for a person.

Operation and event names are per area (`get_<area>_operation`,
`<area>_operation_changed`): the protocol requires every name to belong to one
namespace.

## What each namespace shows

| namespace | queries | notes |
| --- | --- | --- |
| diagnostics | `get_usage_summary`, `get_run_context`, `get_diagnostics_operation` | usage totals in micro-USD with unpriced tokens and unreported requests kept apart (the cost is a lower bound); a run's saved context as an overview, token budget by role and one summary line per message (no message body) |
| memory | `list_memory`, `get_memory`, `get_memory_operation` | lessons of the project (the repository slug is resolved on the worker); search words are quoted literals; revision = status rows + favorite changes; actions listed as `Allowed`/`Denied` with a `DenialReason` |
| improvement | `list_improvement`, `get_improvement`, `get_improvement_operation` | candidates, the settings switch (`enabled`), the draft by **file name only**; the revision is a digest of the fields an update or a new occurrence changes |
| evaluation | `list_evaluations`, `get_evaluation`, `get_evaluation_report`, `get_evaluation_operation` | `Present` / `Missing` / `Problem` are three states; the inspector confines paths, follows no symbolic link and bounds reads; no host path reaches a reply |

Every body is bounded (`BoundedText`, 64 KiB) and masked for secrets. A list is a
`Page` cut by the host's `Pager`; a cursor is bound to its operation and snapshot.

## Client

`crates/gui/src/core_client/features/{diagnostics,memory,improvement,evaluation}.rs`
are read-only features that need `Capability::Queries`. They share
`features/diagnostics/read.rs` (the client side of an operation): a generation
guard drops the answer to an earlier ask (another project, a replaced connection),
a settling event asks again once, and a `Pending` reply never undoes a settled one.
The older panes (`panes/memory.rs`, `self_improvement.rs`, `model/evaluations.rs`)
are untouched.

## Not in this PR

- Update commands and their DTOs (PR 14 memory / improvement, PR 17c evaluation).
- A run's message bodies and the Context Preview (`preview_base_context`); only
  summaries are carried.
- The replay body of an evaluation member (only whether it exists).
- The evaluation launch monitor is not wired, so the process state of a member the
  core launched itself reads from its files and may stay `Unverified`.
- Learning candidates (07b-2b, #613) are not shown; this PR adds no storage API.
- The cache end-to-end scenarios are not extended: these reads send nothing to a
  provider and change no prompt prefix.

## Central tests that change with this PR

A feature that becomes implemented changes what two shared tests assume. They are
central files, so they are fixed in an integration PR, not here:

- `crates/gui/src/core_client/features/tests.rs`: the list of implemented
  features and the "a placeholder sends nothing" check.
