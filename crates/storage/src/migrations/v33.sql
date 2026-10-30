-- Start of the bounded wait for a WAL-only admission refusal (ADR 0041 R5):
-- the instant the current wait began, NULL while the job is not waiting. The
-- deadline of each request is max(this, created_at_ns) + the wait limit, so a
-- restart or an owner change never extends it.
ALTER TABLE thread_index_state ADD COLUMN wal_wait_since_ns INTEGER;
