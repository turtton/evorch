-- Archived-thread index exclusion (PR-2).
--
-- Runs of an archived thread are evicted from the conversation index with the
-- reason 'archived' and their blocks are purged in bounded maintenance steps.
-- This partial index finds the evicted runs that still hold blocks or charges.
CREATE INDEX idx_thread_index_runs_archived_pending ON thread_index_runs(run_id)
    WHERE state = 'evicted' AND evicted_reason = 'archived' AND charged_bytes > 0;

-- Threads archived before this version were never evicted from the index (the mirror
-- hook only acts on a change of the flag). The maintenance tick evicts their runs in
-- bounded steps and records its position here; no data is rewritten by this migration.
ALTER TABLE thread_index_state ADD COLUMN archive_backfill_done INTEGER NOT NULL DEFAULT 0
    CHECK (archive_backfill_done IN (0, 1));
ALTER TABLE thread_index_state ADD COLUMN archive_backfill_cursor TEXT;
-- Nothing to visit when no thread is archived yet (every fresh database), so the
-- first maintenance tick writes nothing.
UPDATE thread_index_state SET archive_backfill_done =
    NOT EXISTS (SELECT 1 FROM threads WHERE archived = 1) WHERE id = 1;
