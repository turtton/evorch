-- Archived-thread index exclusion (PR-2).
--
-- Runs of an archived thread are evicted from the conversation index with the
-- reason 'archived' and their blocks are purged in bounded maintenance steps.

-- A run with a block above the purge step limit is handed to the exclusive phase
-- (`reclaim_oversized`) and flagged here, so the maintenance tick leaves it alone
-- without having to look it up elsewhere.
ALTER TABLE thread_index_runs ADD COLUMN purge_deferred INTEGER NOT NULL DEFAULT 0
    CHECK (purge_deferred IN (0, 1));
-- The runs the maintenance tick still has to purge. The tick reads this index from
-- its start, so every entry it sees is work (no entry is skipped by a filter).
CREATE INDEX IF NOT EXISTS idx_thread_index_runs_archived_pending ON thread_index_runs(run_id)
    WHERE state = 'evicted' AND evicted_reason = 'archived' AND charged_bytes > 0
        AND purge_deferred = 0;
-- Keyset walks of the backfill: archived threads in ID order, and the runs of one
-- thread in ID order, each one index range with no sort and no filtering.
CREATE INDEX IF NOT EXISTS idx_threads_archived_by_id ON threads(thread_id) WHERE archived = 1;
CREATE INDEX IF NOT EXISTS idx_thread_runs_thread_run ON thread_runs(thread_id, run_id);

-- Threads archived before this version were never evicted from the index (the mirror
-- hook only acts on a change of the flag). The maintenance tick evicts their runs in
-- bounded steps and records its position here; no data is rewritten by this migration.
-- The position is the next thread and, inside it, the last run done ('' = none yet).
ALTER TABLE thread_index_state ADD COLUMN archive_backfill_done INTEGER NOT NULL DEFAULT 0
    CHECK (archive_backfill_done IN (0, 1));
ALTER TABLE thread_index_state ADD COLUMN archive_backfill_thread TEXT;
ALTER TABLE thread_index_state ADD COLUMN archive_backfill_run TEXT NOT NULL DEFAULT '';
-- Nothing to visit when no thread is archived yet (every fresh database), so the
-- first maintenance tick writes nothing.
UPDATE thread_index_state SET archive_backfill_done =
    NOT EXISTS (SELECT 1 FROM threads WHERE archived = 1) WHERE id = 1;
