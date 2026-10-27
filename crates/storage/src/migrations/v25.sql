-- Delivery receipts for host chat input (one row per GUI submission).
--
-- A receipt is the authority on whether a submission reached the agent:
-- 'accepted' when the runtime took it, 'delivered' once a run context
-- snapshot containing it committed, 'dropped' when its run ended before
-- reading it, and 'unknown' when the runtime could not tell (for example a
-- terminal snapshot that failed to commit). Events only notify.
CREATE TABLE host_input_receipts (
    prompt_id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL,
    root_run_id TEXT NOT NULL,
    text TEXT NOT NULL,
    image_count INTEGER NOT NULL DEFAULT 0 CHECK (image_count >= 0),
    status TEXT NOT NULL CHECK (status IN ('accepted', 'delivered', 'dropped', 'unknown')),
    reason TEXT,
    accepted_at_ns INTEGER NOT NULL,
    status_at_ns INTEGER NOT NULL
);
CREATE INDEX host_input_receipts_run ON host_input_receipts(run_id, status);

-- Prompt IDs of the host inputs a snapshot's messages contain. A committed
-- snapshot keeps every ID it ever held, including inputs a compaction summarized.
ALTER TABLE run_contexts ADD COLUMN host_inputs_json TEXT NOT NULL DEFAULT '[]';
