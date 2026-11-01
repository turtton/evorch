-- Archived-thread index exclusion (PR-2).
--
-- Runs of an archived thread are evicted from the conversation index with the
-- reason 'archived' and their blocks are purged in bounded maintenance steps.
-- This partial index finds the evicted runs that still hold blocks or charges.
CREATE INDEX idx_thread_index_runs_archived_pending ON thread_index_runs(run_id)
    WHERE state = 'evicted' AND evicted_reason = 'archived' AND charged_bytes > 0;

-- Threads archived before this version were never evicted from the index (the
-- mirror hook only acts on a change of the flag). Evict their runs once; the
-- blocks are removed by the maintenance purge.
UPDATE thread_index_runs SET state = 'evicted', evicted_reason = 'archived',
    next_attempt_at_ns = NULL, blocked_reason = NULL, work_generation = work_generation + 1
WHERE state IN ('pending', 'indexed', 'retry_wait', 'blocked') AND run_id IN (
    SELECT tr.run_id FROM thread_runs tr JOIN threads t ON t.thread_id = tr.thread_id
    WHERE t.archived = 1
    UNION SELECT root_run_id FROM threads WHERE archived = 1 AND root_run_id IS NOT NULL);
