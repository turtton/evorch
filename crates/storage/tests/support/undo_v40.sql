-- Reverts the event body format (migration 41): events lose `body_format` and
-- `blob_bytes`, the event reference table goes, and the three accounting triggers are the
-- ones migration v12 created. Tokenized rows are not turned back into plain ones; tests
-- that need an older schema hold only plain events.
DROP TRIGGER IF EXISTS content_blob_refs_event_queue_gc;
DROP TABLE IF EXISTS content_blob_refs_event;
DROP TRIGGER events_accounting_insert;
DROP TRIGGER events_accounting_delete;
DROP TRIGGER events_accounting_update;
ALTER TABLE events DROP COLUMN body_format;
ALTER TABLE events DROP COLUMN blob_bytes;
CREATE TRIGGER events_accounting_insert AFTER INSERT ON events
BEGIN
    INSERT INTO event_session_bytes (session_id, payload_bytes)
    SELECT NEW.session_id, OCTET_LENGTH(NEW.payload) WHERE NEW.session_id IS NOT NULL
    ON CONFLICT(session_id) DO UPDATE
        SET payload_bytes = payload_bytes + excluded.payload_bytes;
    INSERT INTO event_day_bytes (utc_day, payload_bytes)
    VALUES (NEW.wall_clock_ns / 86400000000000 - (NEW.wall_clock_ns % 86400000000000 < 0),
            OCTET_LENGTH(NEW.payload))
    ON CONFLICT(utc_day) DO UPDATE
        SET payload_bytes = payload_bytes + excluded.payload_bytes;
END;
CREATE TRIGGER events_accounting_delete AFTER DELETE ON events
BEGIN
    UPDATE event_session_bytes SET payload_bytes = payload_bytes - OCTET_LENGTH(OLD.payload)
    WHERE session_id = OLD.session_id;
    DELETE FROM event_session_bytes WHERE session_id = OLD.session_id AND payload_bytes = 0;
    UPDATE event_day_bytes SET payload_bytes = payload_bytes - OCTET_LENGTH(OLD.payload)
    WHERE utc_day = OLD.wall_clock_ns / 86400000000000 - (OLD.wall_clock_ns % 86400000000000 < 0);
    DELETE FROM event_day_bytes
    WHERE utc_day = OLD.wall_clock_ns / 86400000000000 - (OLD.wall_clock_ns % 86400000000000 < 0)
        AND payload_bytes = 0;
END;
CREATE TRIGGER events_accounting_update AFTER UPDATE OF session_id, wall_clock_ns, payload ON events
WHEN NEW.session_id IS NOT OLD.session_id OR NEW.wall_clock_ns != OLD.wall_clock_ns
    OR NEW.payload != OLD.payload
BEGIN
    UPDATE event_session_bytes SET payload_bytes = payload_bytes - OCTET_LENGTH(OLD.payload)
    WHERE session_id = OLD.session_id;
    DELETE FROM event_session_bytes WHERE session_id = OLD.session_id AND payload_bytes = 0;
    UPDATE event_day_bytes SET payload_bytes = payload_bytes - OCTET_LENGTH(OLD.payload)
    WHERE utc_day = OLD.wall_clock_ns / 86400000000000 - (OLD.wall_clock_ns % 86400000000000 < 0);
    DELETE FROM event_day_bytes
    WHERE utc_day = OLD.wall_clock_ns / 86400000000000 - (OLD.wall_clock_ns % 86400000000000 < 0)
        AND payload_bytes = 0;
    INSERT INTO event_session_bytes (session_id, payload_bytes)
    SELECT NEW.session_id, OCTET_LENGTH(NEW.payload) WHERE NEW.session_id IS NOT NULL
    ON CONFLICT(session_id) DO UPDATE
        SET payload_bytes = payload_bytes + excluded.payload_bytes;
    INSERT INTO event_day_bytes (utc_day, payload_bytes)
    VALUES (NEW.wall_clock_ns / 86400000000000 - (NEW.wall_clock_ns % 86400000000000 < 0),
            OCTET_LENGTH(NEW.payload))
    ON CONFLICT(utc_day) DO UPDATE
        SET payload_bytes = payload_bytes + excluded.payload_bytes;
END;

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
