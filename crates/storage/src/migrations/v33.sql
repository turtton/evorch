-- Start of the bounded wait for a WAL-only admission refusal (ADR 0041 R5):
-- the instant the current wait began, NULL while the job is not waiting. The
-- deadline of each request is max(this, created_at_ns) + the wait limit, so a
-- restart or an owner change never extends it.
ALTER TABLE thread_index_state ADD COLUMN wal_wait_since_ns INTEGER;

-- Owner heartbeats and wait-deadline probes must not scan terminal history.
CREATE INDEX idx_thread_index_requests_state_created
    ON thread_index_requests(state, created_at_ns);
