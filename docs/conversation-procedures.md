# Conversation plans and goals

`plan_read` and `plan_update` optionally manage the hierarchical plan and
progress for a user-facing conversation. They are published only to the
conversation's trusted root `Worker` or `Orchestrator` (no parent run,
`conversation: true`, `RunPurpose::General`), including when the conversation
has no goal. Delegated agents report their results to that root. The catalog
change from the retired todo tool is only the addition of `plan_read` /
`plan_update` and the removal of `todo_write`; the effective tool catalog does
not change after a run starts or across a same-build restart.

## Retired `todo_write` and legacy Todo data

`todo_write` is retired and absent from every tool catalog. Legacy Todo data
(`ThreadTodoUpdated` events and `lane_handoffs.todo_json`, dropped in migration
v67) is never converted into a conversation plan and is excluded from state
restoration, GUI projection, and handoff input. Remaining `ThreadTodoUpdated`
rows are neither decoded nor deleted in bulk at startup; bounded thread
retention reclaims them physically over time.

Historical `todo_write` calls in saved transcripts are never re-executed or
regenerated. Any new invocation of `todo_write` leaves state unchanged and
returns the fixed error:
`` `todo_write`は廃止されました。現在の計画には`plan_read`と`plan_update`を使用してください。旧Todoデータは引き継がれず、この呼出しによる変更はありません。 ``

## Reading and updating a plan (`plan_read` / `plan_update`)

A plan is a bounded tree of tasks (up to `PLAN_MAX_NODES = 128` nodes, maximum
depth `PLAN_MAX_DEPTH = 8` with top-level tasks at depth 1, and
`PLAN_MAX_BYTES = 128 KiB` of total serialized task details). Each task's
serialized detail is capped at `14 KiB` (`PLAN_NODE_DETAIL_MAX_BYTES`) so a
single-task read always fits within the `16 KiB` tool result cap
(`PLAN_TOOL_RESULT_MAX_BYTES`). Field byte caps are `title <= 256`,
`description <= 4096`, `result <= 4096`, `blocker <= 2048`, and
`client_ref <= 64`. Task text is data, not instructions, acceptance criteria,
or execution permission.

Only leaf tasks hold a status (`pending`, `in_progress`, `completed`,
`blocked`, or runtime-only `interrupted`). Parent tasks hold no status of their
own; their progress is derived from their children using relative sibling
weights (`weight` integer in `1..=100`, progress in parts per million). A leaf
that gains its first child loses its status; a parent that loses its last child
must be given an explicit leaf status in the same batch.

`plan_read` selects one view:

- `view: "tree"` (`cursor`, `limit` default 50, `1..=100`, page capped at
  `12 KiB`) returns `plan_id`, `revision`, `progress`, a lightweight page of
  `nodes`, and `next_cursor`. If the plan changes while paging, the old cursor
  is rejected with `stale_cursor`.
- `view: "task"` (`task_id`) returns `plan_id`, `revision`, and the full `node`
  detail (`title`, `description`, `result`, `blocker`, `weight`, `status`,
  `progress_ppm`).

`plan_update` takes `expected_revision` (`0` before a plan exists, otherwise
the latest `revision` from `plan_read` or `plan_update`) and `operations` (`1`
to `32` operations, `PLAN_MAX_OPERATIONS`), applying the batch atomically:

- `add`: `{op: "add", client_ref, title, parent?, position?, description?, weight?, status?}`.
  Runtime assigns a monotonic `<plan_id>:<number>` ID that is never reused even
  after `clear`. Subsequent operations in the same batch reference the new task
  with `{"ref": client_ref}`; existing tasks use `{"id": task_id}`.
- `update`: `{op: "update", task, title?, description?, result?, blocker?, weight?, status?}`
  updates at least one field on an existing task (and can move an `interrupted`
  leaf back to any model-writable status).
- `move`: `{op: "move", task, parent?, position?}` repositions a task without
  creating a cycle or exceeding depth 8.
- `remove`: `{op: "remove", task}` removes the task and its subtree.
- `clear`: `{op: "clear"}` removes all tasks; it must be the only operation in
  the batch. Change history and the monotonic task counter are retained.

A stale `expected_revision`, unknown field, invalid reference, or limit
violation rejects the entire batch without changing state; the model must
re-read before retrying. A batch whose resulting tree is identical to the
starting tree is a no-op (`changed: false`) that neither increments `revision`
nor emits an event. Every effective change increments `revision` by 1 and
records a history entry.

## `interrupted` reconciliation and ownership

`interrupted` is runtime-only: model batches that name it are refused. It means
work was in progress when the root's work period ended before completion was
recorded, not task failure or a stopped child lane. Reconciliation
(`reconcile_root_plan` / `reconcile_finished_turn`) transitions all current
`in_progress` leaves to `interrupted` (exempt from the 32-operation model batch
cap) and leaves other statuses intact.

Reconciliation runs at a completed turn with no unanswered blocking question
(including restoration of a saved completed turn), and on natural termination,
accepted finish, Stop, Cancel, error, or panic. It does not run for a
blocking-question wait, refused finish, tool or child wait, compaction, model
switch, lane notification receipt or ACK, or successful escalation (ownership
handoff). If reconciliation fails, the plan is left unchanged and a
`CONVERSATION_PLAN_RECONCILE_FAILED` warning diagnostic is emitted without
altering how the run ends.

Each thread's current trusted root owns and reconciles only its own plan. Lane
activity does not propagate, automatically transfer, or synchronize plans
between threads.

## Durability and one-time switch notice

The plan is durable conversation state. Restart, compaction, and escalation
preserve its latest contents (including an explicit `clear` and its history);
handoff preserves the thread's `plan_id`, while a fork derives a new `plan_id`.
Restoring a thread loads projection data without granting an old run write
authority. Reconciled changes reach the model through `plan_read` or the
one-time switch notice below, never by rewriting previously sent provider input,
interleaving with an unfinished tool-result sequence, or creating an extra
provider request or wake.

Only conversations saved before the switch (a saved `RunRestoreDescriptor`
without `plan_notice`) and fork/rewind seeds start with `PlanNotice::Pending`.
Conversations started after the switch start with `PlanNotice::Done` and keep
their first provider request unchanged. When `PlanNotice::Pending` is active on
a trusted conversation root, at a completed tool-round boundary right before the
next provider request that is needed anyway, the runtime appends a one-time
`[runtime conversation plan state]` user notice stating that the retired todo
tool and its lists were not carried over, followed by either `"No plan exists
yet for this conversation."` or the current `revision`, task count, and leaf
status counts, and marks `plan_notice` as `Done`.

If the restored run is crossing a verified planned tool-schema handoff boundary
whose boundary request has not yet received its first response
(`boundary_pending` or consumed restart-tools boundary evidence), `plan_notice`
is persisted as `PlanNotice::Deferred` so the schema-boundary request and any
interrupted retry of it stay notice-free. Once that boundary request receives a
response, `PlanNotice::Deferred` transitions to `PlanNotice::Pending` for the
next ordinary boundary.

## Independence from goals

A plan is working memory: it does not enforce continued work, restart stopped
agents, run a review, or complete a goal. Ordinary research, editing, or
verification does not require either mechanism. A goal is appropriate when the
user's request clearly calls for continued work until an outcome is reached and
checked against evidence. Goals retain their own acceptance criteria, checks,
and optional review.

## GUI progress band and `ConversationPlan` panel

The composer uses one shared inset band above the input (below any queued
follow-up rows). A goal occupies the band's summary row when present, and the
conversation plan occupies the progress row (`PlanBand`) directly below it;
either row can appear alone. Goal details expand inline within the band's
bounded scroll area, and goal review / check-pause controls stay on the goal
row; Stop in the composer controls agent execution.

The plan row in the composer band is read-only and displays the server's
`PlanProgress` without client-side recomputation: overall status icon, weighted
progress bar, completion percentage, completed leaf count
(`completed/leaf_count`), any nonzero `blocked` or `interrupted` leaf counts,
and an `Open plan` button on the right (`"Not planned"` with the button
disabled before any plan exists; `"Plan cleared"` with the button enabled when
all tasks were cleared so history remains reachable). Stored `in_progress` is
not an animation or proof that a stopped agent is still running.

Pressing `Open plan` focuses or opens the on-demand, read-only
`ConversationPlan` panel (`conversation-plan-main`). The panel shows:

- A top summary bar with weighted status shares, completion percentage and leaf
  count, a five-status weight legend (`Completed`, `In progress`,
  `Interrupted`, `Pending`, `Blocked`), and `r<revision> · HH:MM · <run_id>`
  metadata.
- The hierarchical task tree (indented by depth, status icon, parent mini
  progress bar and completion share, and sibling weight).
- The selected task's detail (`title`, highlighted `blocker`, `description`,
  and `result`) together with paged change history (`Added`, `Updated`,
  `Moved`, `Removed`, `Cleared`, local timestamp, and cause / source run). When
  a plan has been cleared, the panel shows the `"Plan cleared"` notice and its
  paged history.
