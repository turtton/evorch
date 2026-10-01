-- Compact authoritative totals avoid scanning event payloads when another writer
-- commits. Backfill once; every later event mutation updates these in its own
-- transaction. WITHOUT ROWID keeps each session total in a single B-tree.
CREATE TABLE event_session_bytes (
    session_id TEXT PRIMARY KEY,
    payload_bytes INTEGER NOT NULL
        CHECK (typeof(payload_bytes) = 'integer' AND payload_bytes >= 0)
) WITHOUT ROWID;
CREATE TABLE event_day_bytes (
    utc_day INTEGER PRIMARY KEY,
    payload_bytes INTEGER NOT NULL
        CHECK (typeof(payload_bytes) = 'integer' AND payload_bytes >= 0)
);
INSERT INTO event_session_bytes
SELECT session_id, SUM(OCTET_LENGTH(payload)) FROM events
WHERE session_id IS NOT NULL GROUP BY session_id;
-- SQLite integer division truncates toward zero. Floor negative timestamps so
-- pre-epoch raw SQL events never contribute to the epoch day's quota.
INSERT INTO event_day_bytes
SELECT wall_clock_ns / 86400000000000 - (wall_clock_ns % 86400000000000 < 0),
       SUM(OCTET_LENGTH(payload)) FROM events GROUP BY 1;

-- REPLACE silently omits DELETE triggers with recursive_triggers=OFF. Reject
-- conflicting event IDs instead of allowing an unaccounted replacement.
CREATE TRIGGER events_accounting_no_replace BEFORE INSERT ON events
WHEN NEW.id != -1 AND EXISTS (SELECT 1 FROM events WHERE id = NEW.id)
BEGIN SELECT RAISE(ABORT, 'event IDs cannot be replaced'); END;
-- BEFORE INSERT uses -1 for an unassigned rowid. Check this reserved ID after
-- assignment instead, so a legacy raw id=-1 never blocks automatic appends.
CREATE TRIGGER events_accounting_reserved_id AFTER INSERT ON events
WHEN NEW.id = -1
BEGIN SELECT RAISE(ABORT, 'event ID -1 is reserved'); END;
CREATE TRIGGER events_accounting_no_update_replace BEFORE UPDATE OF id ON events
WHEN NEW.id != OLD.id AND EXISTS (SELECT 1 FROM events WHERE id = NEW.id)
BEGIN SELECT RAISE(ABORT, 'event IDs cannot be replaced'); END;

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
