-- Explicit offline downgrade fixture: a request still registering never became
-- visible, so it is dropped with its targets.
DELETE FROM thread_index_request_targets WHERE request_id IN (
    SELECT request_id FROM thread_index_requests WHERE state = 'registering');
CREATE TABLE thread_index_requests_undo68 (
    request_id TEXT PRIMARY KEY NOT NULL,
    canonical_project TEXT,
    since_ns INTEGER,
    include_evicted INTEGER NOT NULL CHECK (include_evicted IN (0, 1)),
    state TEXT NOT NULL CHECK (state IN (
        'pending', 'completed', 'completed_with_gaps', 'failed', 'cancelled')),
    failure_reason TEXT,
    deleted_before_submission INTEGER NOT NULL DEFAULT 0
        CHECK (deleted_before_submission >= 0),
    created_at_ns INTEGER NOT NULL,
    finished_at_ns INTEGER,
    CHECK ((state = 'failed') = (failure_reason IS NOT NULL)),
    CHECK ((state = 'pending') = (finished_at_ns IS NULL))
);
INSERT INTO thread_index_requests_undo68(
    request_id, canonical_project, since_ns, include_evicted, state, failure_reason,
    deleted_before_submission, created_at_ns, finished_at_ns)
SELECT request_id, canonical_project, since_ns, include_evicted, state, failure_reason,
    deleted_before_submission, created_at_ns, finished_at_ns
FROM thread_index_requests WHERE state <> 'registering';
DROP TABLE thread_index_requests;
ALTER TABLE thread_index_requests_undo68 RENAME TO thread_index_requests;
CREATE INDEX idx_thread_index_requests_state_created
    ON thread_index_requests(state, created_at_ns);
