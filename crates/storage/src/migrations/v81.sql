-- Row-local recovery format boundary. Existing recipes are never converted.
ALTER TABLE restart_transactions ADD COLUMN format_version INTEGER NOT NULL DEFAULT 1;
-- owner_json is now only the trusted root binding. Preserve Stop, claims and ACKs.
-- Malformed bindings remain diagnosable and do not prevent opening the database.
UPDATE lane_delivery_destinations SET owner_json=json_remove(owner_json,'$.ownership_generation')
WHERE CASE WHEN json_valid(owner_json) THEN json_type(owner_json)='object' ELSE 0 END;
-- Old generation-keyed progress is not evidence for an owner-free disposal.
-- Retain it verbatim for diagnosis; never copy completion into the new table.
ALTER TABLE recovery_graph_thread_progress RENAME TO recovery_graph_thread_progress_v1;
CREATE TABLE recovery_graph_thread_progress (
 disposal_key TEXT PRIMARY KEY NOT NULL,
 manifest_id TEXT NOT NULL,
 restart_id TEXT NOT NULL,
 capture_id TEXT NOT NULL,
 thread_id TEXT NOT NULL,
 revision INTEGER NOT NULL CHECK(revision > 0),
 operation_id TEXT NOT NULL,
 attempt_nonce TEXT NOT NULL,
 scratch TEXT NOT NULL CHECK(scratch IN ('not_started','applied','not_applicable','unknown')),
 UNIQUE(manifest_id,restart_id,capture_id,thread_id)
);
