# Learning from completed thread goals

The runtime admits learning at the actual `Complete` transition, after either
self-check or independent review. Escalation, ordinary conversation turns,
`Blocked`, and replay of a saved complete goal are not new triggers.
Goal creation policy is unchanged. The existing finite root `Done` learning
path remains available.

## Evidence boundary

Before the conversation accepts another input, the loop saves its current safe
context and captures `root_run_id`, `related_root_run_ids`, and their persisted
descendants in one SQLite read transaction. Limits remain 128 runs / 16 MiB,
across the entire lineage; exceeding a limit fails closed, not by truncation.
An immutable, redacted source snapshot is installed before the extractor starts.
Extraction and independent lesson review share it, including the original
record references (`run@revision-time:message:block`). Later writes, compaction,
or resumed conversation cannot shift those references. The completed goal's
objective, criteria, checks/evidence, and original request accompany the internal
prompt.

Preparing evidence does not count as inspecting it. The extractor must successfully
call `inspect_learning_source`; a successful listing with no candidates is valid,
but ending without inspection fails the pipeline and emits its failure diagnostic.

Only the runtime-issued internal learning runs receive this evidence authority.
They cannot recursively trigger learning; child runs cannot admit pipelines.
Neither source conversation messages nor its system/tools/cache affinity are
modified by learning.

## Durable admission

`learning_claims` stores the unique task key `goal-learning:{goal_id}:{epoch}`.
An immediate writer transaction atomically checks this key, the UTC daily
admission quota, and legacy-source duplication before recording the claim.
`collect_lessons=false` prevents admission. The configured `daily_limit` is
shared across pipelines in this storage database; without explicit policy,
the existing default of 20 applies.

`learning_claim_roots` records all roots covered by a claim. A legacy `Done`
pipeline cannot re-learn a root covered by goal learning (or another legacy
pipeline). A goal cannot re-learn a root already covered by legacy learning.
Different completed goals may share an interactive root.

Admission is **at-most-once**, not a resumable job queue: a crash after the claim
can lose the extraction, but restoring/restarting does not duplicate it.
Temporary evidence snapshots are released after extraction/review; durable
claims deliberately remain.

## Diagnostics

Snapshot/admission failures and failed learning pipelines emit the existing
`LearningPipelineFailed` diagnostic without changing the conversation result.
An ongoing conversation without a goal does not emit a repeated “no trigger”
warning: goal use is optional and an idle turn is not evidence of a broken
pipeline. Quota/deduplication skips are normal and silent.
