# Conversation procedures and goals

`todo_write` optionally keeps the current procedure for a user-facing conversation.
It is available to its trusted root Worker or Orchestrator, including when the
conversation has no goal. Delegated agents report their results to that root.

Each `todo_write` selects exactly one operation:

- `items` replaces the entire list with `content` and `status` (`pending`,
  `in_progress`, or `completed`). An empty list clears it. Omit obsolete steps
  rather than marking them completed. Use this operation to change text.
- `advance: true` completes the single `in_progress` item, if any, and starts
  the first `pending` item in list order. Multiple in-progress items are
  ambiguous and require `update`. With none in progress, any `interrupted`
  item requires an explicit decision through `update`; an empty list is an error.
- `update: {index, status}` sets one existing item's status by zero-based index.
  It can return an interrupted item to any model-writable status.

`advance` and `update` require `expected_revision` from the latest result;
`items` must omit it. A stale revision returns the current revision and a bounded
list summary without changing anything. Invalid or mixed operations, invalid
indices, missing lists and revision exhaustion also leave state unchanged.
Updating to the same status or advancing an already completed nonempty list is a
successful no-op (`changed: false`), with no revision increment or update event.
Replacements and other changes increment the revision. Parallel work may have
more than one item in progress. Completed lists remain until replaced or cleared.

`interrupted` is runtime-only: models cannot write it. It means work started but
the root's work period ended before completion was recorded, not failure or a
stopped lane. Reconciliation changes all current `in_progress` items to
`interrupted`, leaving other statuses intact. It runs at a completed turn with
no unanswered blocking question, including restoration of a saved completed turn,
and on natural termination, accepted finish, Stop, Cancel, error or panic.
It does not run for a blocking-question wait, refused finish, tool or child wait,
compaction, model switch, lane notification receipt or ACK, or successful
escalation (ownership handoff). Reconciliation failure leaves the list unchanged
and reports a diagnostic without changing how the run ends.

Each thread's current trusted root owns and reconciles only its own list. Lane
activity does not propagate, automatically transfer or synchronize procedures
between threads.

The procedure is durable conversation state. Restart, compaction and escalation
must preserve its latest contents, including an explicit clear. Restore loads
data without granting an old run write authority. The model receives current
state at the next applicable context boundary, without rewriting previously sent
provider input or interleaving context with an unfinished tool-result sequence.
Reconciled state is appended to the next request that would otherwise occur;
it never creates a request, wakes a run, forces continuation or rejects finish.

A procedure does not enforce continued work, restart stopped agents, run a
review, or complete a goal. Ordinary research, editing or verification does not
require either mechanism. A goal is appropriate when the user's request clearly
calls for continued work until an outcome is reached and checked against evidence.
Goals retain their own acceptance criteria, checks and optional review.

The composer uses one shared inset band. A goal occupies its first summary row;
the procedure occupies the second, showing completed count and the current step.
Either can appear alone. Each row opens its own details in one shared detail
area, with both summaries remaining visible. Procedure statuses are read-only.
Only `completed` counts toward completion. Interrupted items are shown as
unfinished; a list containing only interrupted items is not all completed.
Goal review and check-pause controls remain on the goal row; Stop controls agent
work in the composer. Stored `in_progress` is not an animation or proof that a
stopped agent is still running.
