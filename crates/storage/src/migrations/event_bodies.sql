-- Event bodies as tokens (PR B1): the long strings of Message, compaction and
-- compressible Tool payloads move to `content_blobs`, as the strings of a run's messages
-- did (A2b).
--
-- This migration is the shape of a NEW database and the step the conversion command runs
-- (as the "events prepare" step) on a database that holds the previous format. It changes
-- no existing row: `body_format` 0 is the plain payload every existing row has, and
-- `blob_bytes` 0 is what an event that owns no new blob is charged.

-- `body_format`: 0 the payload is the event's JSON (plain TEXT, or a zstd BLOB for the
-- compressible kinds); 1 the payload is the event's JSON in which every string that needs
-- a token is replaced by its token (only kinds `compressible_payload` accepts).
-- `blob_bytes`: the stored bytes of the blobs this event created when it was first
-- stored in token form. It is an accounting figure, not the physical size: a blob another
-- event or a run created first costs this event nothing, and deleting the creator does
-- not move the charge to the other owners.
ALTER TABLE events ADD COLUMN body_format INTEGER NOT NULL DEFAULT 0 CHECK (body_format IN (0, 1));
ALTER TABLE events ADD COLUMN blob_bytes INTEGER NOT NULL DEFAULT 0 CHECK (blob_bytes >= 0);

-- Reference table, in the shape `OwnerSpec::ref_table_ddl` produces (a test compares).
CREATE TABLE content_blob_refs_event (
    event_id INTEGER NOT NULL,
    blob_id INTEGER NOT NULL REFERENCES content_blobs(id),
    PRIMARY KEY (event_id, blob_id),
    FOREIGN KEY (event_id) REFERENCES events(id) ON DELETE CASCADE
) WITHOUT ROWID;
CREATE INDEX idx_content_blob_refs_event_blob ON content_blob_refs_event(blob_id);
CREATE TRIGGER content_blob_refs_event_queue_gc AFTER DELETE ON content_blob_refs_event
BEGIN
    INSERT OR IGNORE INTO content_blob_gc_queue(blob_id) VALUES (OLD.blob_id);
END;

-- Accounting: an event costs `OCTET_LENGTH(payload) + blob_bytes`. The three persistent
-- totals (`event_session_bytes`, `event_day_bytes`, `sessions.total_event_bytes`) are
-- maintained here and only here, so no writer adjusts them by hand and a total cannot be
-- counted twice. (`events_accounting_no_replace`, `_reserved_id` and `_no_update_replace`
-- of migration v12 are unchanged.)
DROP TRIGGER events_accounting_insert;
DROP TRIGGER events_accounting_delete;
DROP TRIGGER events_accounting_update;

CREATE TRIGGER events_accounting_insert AFTER INSERT ON events
BEGIN
    INSERT INTO event_session_bytes (session_id, payload_bytes)
    SELECT NEW.session_id, OCTET_LENGTH(NEW.payload) + NEW.blob_bytes WHERE NEW.session_id IS NOT NULL
    ON CONFLICT(session_id) DO UPDATE
        SET payload_bytes = payload_bytes + excluded.payload_bytes;
    INSERT INTO event_day_bytes (utc_day, payload_bytes)
    VALUES (NEW.wall_clock_ns / 86400000000000 - (NEW.wall_clock_ns % 86400000000000 < 0),
            OCTET_LENGTH(NEW.payload) + NEW.blob_bytes)
    ON CONFLICT(utc_day) DO UPDATE
        SET payload_bytes = payload_bytes + excluded.payload_bytes;
    UPDATE sessions
    SET total_event_bytes = total_event_bytes + OCTET_LENGTH(NEW.payload) + NEW.blob_bytes
    WHERE id = NEW.session_id;
END;

CREATE TRIGGER events_accounting_delete AFTER DELETE ON events
BEGIN
    UPDATE event_session_bytes
    SET payload_bytes = payload_bytes - OCTET_LENGTH(OLD.payload) - OLD.blob_bytes
    WHERE session_id = OLD.session_id;
    DELETE FROM event_session_bytes WHERE session_id = OLD.session_id AND payload_bytes = 0;
    UPDATE event_day_bytes
    SET payload_bytes = payload_bytes - OCTET_LENGTH(OLD.payload) - OLD.blob_bytes
    WHERE utc_day = OLD.wall_clock_ns / 86400000000000 - (OLD.wall_clock_ns % 86400000000000 < 0);
    DELETE FROM event_day_bytes
    WHERE utc_day = OLD.wall_clock_ns / 86400000000000 - (OLD.wall_clock_ns % 86400000000000 < 0)
        AND payload_bytes = 0;
    UPDATE sessions
    SET total_event_bytes = MAX(0, total_event_bytes - OCTET_LENGTH(OLD.payload) - OLD.blob_bytes)
    WHERE id = OLD.session_id;
END;

CREATE TRIGGER events_accounting_update
AFTER UPDATE OF session_id, wall_clock_ns, payload, blob_bytes ON events
WHEN NEW.session_id IS NOT OLD.session_id OR NEW.wall_clock_ns != OLD.wall_clock_ns
    OR NEW.payload != OLD.payload OR NEW.blob_bytes != OLD.blob_bytes
BEGIN
    UPDATE event_session_bytes
    SET payload_bytes = payload_bytes - OCTET_LENGTH(OLD.payload) - OLD.blob_bytes
    WHERE session_id = OLD.session_id;
    DELETE FROM event_session_bytes WHERE session_id = OLD.session_id AND payload_bytes = 0;
    UPDATE event_day_bytes
    SET payload_bytes = payload_bytes - OCTET_LENGTH(OLD.payload) - OLD.blob_bytes
    WHERE utc_day = OLD.wall_clock_ns / 86400000000000 - (OLD.wall_clock_ns % 86400000000000 < 0);
    DELETE FROM event_day_bytes
    WHERE utc_day = OLD.wall_clock_ns / 86400000000000 - (OLD.wall_clock_ns % 86400000000000 < 0)
        AND payload_bytes = 0;
    UPDATE sessions
    SET total_event_bytes = MAX(0, total_event_bytes - OCTET_LENGTH(OLD.payload) - OLD.blob_bytes)
    WHERE id = OLD.session_id;
    INSERT INTO event_session_bytes (session_id, payload_bytes)
    SELECT NEW.session_id, OCTET_LENGTH(NEW.payload) + NEW.blob_bytes WHERE NEW.session_id IS NOT NULL
    ON CONFLICT(session_id) DO UPDATE
        SET payload_bytes = payload_bytes + excluded.payload_bytes;
    INSERT INTO event_day_bytes (utc_day, payload_bytes)
    VALUES (NEW.wall_clock_ns / 86400000000000 - (NEW.wall_clock_ns % 86400000000000 < 0),
            OCTET_LENGTH(NEW.payload) + NEW.blob_bytes)
    ON CONFLICT(utc_day) DO UPDATE
        SET payload_bytes = payload_bytes + excluded.payload_bytes;
    UPDATE sessions
    SET total_event_bytes = total_event_bytes + OCTET_LENGTH(NEW.payload) + NEW.blob_bytes
    WHERE id = NEW.session_id;
END;
