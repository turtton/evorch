# Conversation procedures and goals

`todo_write` optionally keeps the current procedure for a user-facing conversation.
It is available to its trusted root Worker or Orchestrator, including when the
conversation has no goal. Delegated agents report their results to that root.

Each write replaces the entire list with `items` containing `content` and
`status` (`pending`, `in_progress`, or `completed`). Parallel work may have more
than one item in progress. Only finished steps are completed; obsolete steps
are removed. An empty list clears the procedure. Completed lists remain available
until the root replaces or clears them.

The procedure is durable conversation state. Restart, compaction and escalation
must preserve its latest contents, including an explicit clear. Restore loads
data without granting an old run write authority. The model receives current
state at the next applicable context boundary, without rewriting previously sent
provider input or interleaving context with an unfinished tool-result sequence.

A procedure does not enforce continued work, restart stopped agents, run a
review, or complete a goal. Ordinary research, editing or verification does not
require either mechanism. A goal is appropriate when the user's request clearly
calls for continued work until an outcome is reached and checked against evidence.
Goals retain their own acceptance criteria, checks and optional review.

The composer uses one shared inset band. A goal occupies its first summary row;
the procedure occupies the second, showing completed count and the current step.
Either can appear alone. Each row opens its own details in one shared detail
area, with both summaries remaining visible. Procedure statuses are read-only.
Goal review and check-pause controls remain on the goal row; Stop controls agent
work in the composer. Stored `in_progress` is not an animation or proof that a
stopped agent is still running.
