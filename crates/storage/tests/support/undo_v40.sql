-- Restores the run_contexts shape migration v40 replaced (the whole conversation in
-- `messages_json`), for tests that rebuild an older schema by hand and replay the
-- migrations from there. Rows keep their headers and get an empty conversation.
DROP TRIGGER IF EXISTS run_messages_drop_refs;
DROP TABLE IF EXISTS content_blob_refs_message;
DROP TABLE IF EXISTS content_blob_refs_header;
DROP TABLE IF EXISTS run_messages;
CREATE TABLE run_contexts_undo (
    run_id TEXT PRIMARY KEY,
    role TEXT NOT NULL,
    name TEXT NOT NULL,
    parent_run_id TEXT,
    config_json TEXT NOT NULL,
    messages_json TEXT NOT NULL,
    checkpoints_json TEXT NOT NULL,
    terminal_phase TEXT NOT NULL,
    restorable INTEGER NOT NULL,
    updated_at_ns INTEGER NOT NULL,
    host_inputs_json TEXT NOT NULL DEFAULT '[]',
    snapshot_revision INTEGER NOT NULL DEFAULT 0 CHECK (snapshot_revision >= 0),
    messages_len INTEGER NOT NULL DEFAULT 0,
    checkpoints_len INTEGER NOT NULL DEFAULT 0
);
INSERT INTO run_contexts_undo
SELECT run_id, role, name, parent_run_id, config_json, '[]', checkpoints_json, terminal_phase,
       restorable, updated_at_ns, host_inputs_json, snapshot_revision, 2, checkpoints_len
FROM run_contexts;
DROP TABLE run_contexts;
ALTER TABLE run_contexts_undo RENAME TO run_contexts;
CREATE INDEX idx_run_contexts_parent ON run_contexts(parent_run_id);
CREATE INDEX idx_run_contexts_chat_thread
    ON run_contexts(substr(name, 6 + instr(substr(name, 6), ':')))
    WHERE parent_run_id IS NULL AND substr(name, 1, 5) = 'chat:'
        AND instr(substr(name, 6), ':') > 1;
CREATE TRIGGER fence_run_contexts_insert BEFORE INSERT ON run_contexts
WHEN NEW.run_id IN (SELECT run_id FROM run_tombstones) OR NEW.parent_run_id IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
CREATE TRIGGER fence_run_contexts_update BEFORE UPDATE ON run_contexts
WHEN NEW.run_id IN (SELECT run_id FROM run_tombstones) OR NEW.parent_run_id IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
