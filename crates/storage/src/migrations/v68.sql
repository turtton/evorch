-- A reindex request is registered in bounded batches (the old single transaction
-- grew with the number of runs): the request row is created first as
-- 'registering', its targets are committed batch by batch, and a last short
-- transaction turns it into 'pending'. A 'registering' request is invisible to
-- every consumer (they read 'pending' only) and a start-up sweep cancels the ones
-- an interrupted invocation left behind. This migration only widens the state
-- set; it changes no existing row.
CREATE TABLE thread_index_requests_v68 (
    request_id TEXT PRIMARY KEY NOT NULL,
    canonical_project TEXT,
    since_ns INTEGER,
    include_evicted INTEGER NOT NULL CHECK (include_evicted IN (0, 1)),
    state TEXT NOT NULL CHECK (state IN (
        'registering', 'pending', 'completed', 'completed_with_gaps', 'failed',
        'cancelled')),
    failure_reason TEXT,
    deleted_before_submission INTEGER NOT NULL DEFAULT 0
        CHECK (deleted_before_submission >= 0),
    created_at_ns INTEGER NOT NULL,
    finished_at_ns INTEGER,
    CHECK ((state = 'failed') = (failure_reason IS NOT NULL)),
    CHECK ((state IN ('registering', 'pending')) = (finished_at_ns IS NULL))
);
INSERT INTO thread_index_requests_v68(
    request_id, canonical_project, since_ns, include_evicted, state, failure_reason,
    deleted_before_submission, created_at_ns, finished_at_ns)
SELECT request_id, canonical_project, since_ns, include_evicted, state, failure_reason,
    deleted_before_submission, created_at_ns, finished_at_ns
FROM thread_index_requests;
DROP TABLE thread_index_requests;
ALTER TABLE thread_index_requests_v68 RENAME TO thread_index_requests;
CREATE INDEX idx_thread_index_requests_state_created
    ON thread_index_requests(state, created_at_ns);
