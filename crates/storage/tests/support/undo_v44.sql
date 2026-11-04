-- Reverts the event body format (migration 44): events lose `body_format` and
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
