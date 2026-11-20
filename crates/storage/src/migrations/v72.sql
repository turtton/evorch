-- CLI continuity (5b-D): the durable record that lets a CLI invocation continue across a
-- planned restart. A row is evidence for a later check, never an authority: it holds the
-- digest of a token (never the token), the invocation and handoff it is bound to, and two
-- deadlines that are fixed by the writer from constants (120 s from issue to Ready, 30 s
-- from Ready to reconnect). Nothing here can set or extend a deadline after it is written;
-- the CHECKs pin the arithmetic and the triggers pin the direction.
CREATE TABLE cli_continuity_grants (
    invocation_id TEXT NOT NULL
        CHECK (length(CAST(invocation_id AS BLOB)) BETWEEN 1 AND 128),
    restart_id TEXT NOT NULL REFERENCES restart_transactions(restart_id),
    capture_id TEXT NOT NULL
        CHECK (length(CAST(capture_id AS BLOB)) BETWEEN 1 AND 128),
    token_digest BLOB NOT NULL CHECK (length(token_digest) = 32),
    issued_at_ms INTEGER NOT NULL CHECK (issued_at_ms >= 0),
    ready_deadline_ms INTEGER NOT NULL CHECK (ready_deadline_ms = issued_at_ms + 120000),
    ready_at_ms INTEGER,
    reconnect_deadline_ms INTEGER,
    state TEXT NOT NULL CHECK (state IN ('issued', 'ready', 'redeemed', 'revoked', 'expired')),
    ended_at_ms INTEGER,
    PRIMARY KEY (invocation_id, restart_id),
    UNIQUE (token_digest),
    CHECK ((ready_at_ms IS NULL) = (reconnect_deadline_ms IS NULL)),
    CHECK (ready_at_ms IS NULL
        OR (ready_at_ms >= issued_at_ms AND ready_at_ms < ready_deadline_ms)),
    CHECK (reconnect_deadline_ms IS NULL OR reconnect_deadline_ms = ready_at_ms + 30000),
    CHECK (state <> 'issued' OR ready_at_ms IS NULL),
    CHECK (state NOT IN ('ready', 'redeemed') OR ready_at_ms IS NOT NULL),
    CHECK ((state IN ('issued', 'ready')) = (ended_at_ms IS NULL)),
    CHECK (state <> 'redeemed' OR ended_at_ms < reconnect_deadline_ms)
);
CREATE INDEX cli_continuity_live ON cli_continuity_grants(state)
    WHERE state IN ('issued', 'ready');
CREATE INDEX cli_continuity_by_restart ON cli_continuity_grants(restart_id);
-- The binding, the digest and the first deadline never change; the Ready time and the
-- reconnect deadline are written once; an ended grant never changes again; the state only
-- moves forward. A raw UPDATE cannot revive or extend a grant either.
CREATE TRIGGER cli_continuity_fixed BEFORE UPDATE ON cli_continuity_grants
WHEN NEW.invocation_id IS NOT OLD.invocation_id
    OR NEW.restart_id IS NOT OLD.restart_id
    OR NEW.capture_id IS NOT OLD.capture_id
    OR NEW.token_digest IS NOT OLD.token_digest
    OR NEW.issued_at_ms IS NOT OLD.issued_at_ms
    OR NEW.ready_deadline_ms IS NOT OLD.ready_deadline_ms
BEGIN SELECT RAISE(ABORT, 'continuity binding and deadline are fixed'); END;
CREATE TRIGGER cli_continuity_deadline_once BEFORE UPDATE ON cli_continuity_grants
WHEN OLD.ready_at_ms IS NOT NULL
    AND (NEW.ready_at_ms IS NOT OLD.ready_at_ms
        OR NEW.reconnect_deadline_ms IS NOT OLD.reconnect_deadline_ms)
BEGIN SELECT RAISE(ABORT, 'continuity deadline cannot be reset'); END;
CREATE TRIGGER cli_continuity_forward_only BEFORE UPDATE ON cli_continuity_grants
WHEN OLD.state IN ('redeemed', 'revoked', 'expired')
    OR NOT ((OLD.state = 'issued' AND NEW.state IN ('issued', 'ready', 'revoked', 'expired'))
        OR (OLD.state = 'ready' AND NEW.state IN ('ready', 'redeemed', 'revoked', 'expired')))
BEGIN SELECT RAISE(ABORT, 'continuity state only moves forward'); END;
CREATE TRIGGER cli_continuity_keep_live BEFORE DELETE ON cli_continuity_grants
WHEN OLD.state IN ('issued', 'ready')
BEGIN SELECT RAISE(ABORT, 'a live continuity grant cannot be deleted'); END;
-- The high-water mark of every time the writer was given. Time handed in below it is a
-- clock regression and is refused, so a clock set back cannot revive or extend a grant.
CREATE TABLE cli_continuity_clock (
    id INTEGER PRIMARY KEY NOT NULL CHECK (id = 1),
    high_water_ms INTEGER NOT NULL CHECK (high_water_ms >= 0)
);
INSERT INTO cli_continuity_clock(id, high_water_ms) VALUES (1, 0);
CREATE TRIGGER cli_continuity_clock_forward BEFORE UPDATE ON cli_continuity_clock
WHEN NEW.high_water_ms < OLD.high_water_ms OR NEW.id <> OLD.id
BEGIN SELECT RAISE(ABORT, 'continuity clock cannot move back'); END;
CREATE TRIGGER cli_continuity_clock_keep BEFORE DELETE ON cli_continuity_clock
BEGIN SELECT RAISE(ABORT, 'continuity clock cannot be removed'); END;
