-- ADR 0044: a close is the lead's (or a human's) judgement, not a verified outcome.
-- lane_closes now has a kind, an optional Reviewer reference and room for the
-- snapshot of what was known when the close was decided. Rebuilt because SQLite
-- cannot relax NOT NULL or a CHECK in place.
DROP TRIGGER lane_close_immutable;
DROP TRIGGER fence_lane_closes_insert;
DROP TRIGGER fence_lane_closes_update;
DROP INDEX lane_closes_lane;
ALTER TABLE lane_closes RENAME TO lane_closes_v56;
CREATE TABLE lane_closes (
 close_id TEXT PRIMARY KEY,
 lane_id TEXT NOT NULL REFERENCES lanes(lane_id) ON DELETE CASCADE,
 verification_id TEXT,
 kind TEXT NOT NULL DEFAULT 'lead_closed' CHECK(kind IN ('lead_closed','human_closed','discarded')),
 payload TEXT NOT NULL CHECK(OCTET_LENGTH(payload) <= 32768)
);
-- A v48 close was a verified close. It stays a lead close; what was never recorded
-- (the actor, artifact observations, the revisions) is left absent, not invented.
INSERT INTO lane_closes(close_id,lane_id,verification_id,kind,payload)
SELECT close_id,lane_id,verification_id,'lead_closed',
 json_object('lane_id',lane_id,'close_id',close_id,'kind','lead_closed',
  'ready_seq',json_extract(payload,'$.ready_seq'),
  'closed_revision',json_extract(payload,'$.closed_revision'),
  'note','migrated from a verified close (v48); the closer was not recorded',
  'outcome','closed')
FROM lane_closes_v56;
DROP TABLE lane_closes_v56;
CREATE INDEX lane_closes_lane ON lane_closes(lane_id);
CREATE TRIGGER lane_close_immutable BEFORE UPDATE ON lane_closes
BEGIN SELECT RAISE(ABORT,'lane close history is immutable'); END;
CREATE TRIGGER fence_lane_closes_insert BEFORE INSERT ON lane_closes
WHEN EXISTS(SELECT 1 FROM lanes WHERE lane_id=NEW.lane_id AND thread_id IN (SELECT thread_id FROM thread_tombstones))
BEGIN SELECT RAISE(ABORT,'run or thread is tombstoned'); END;
CREATE TRIGGER fence_lane_closes_update BEFORE UPDATE ON lane_closes
WHEN EXISTS(SELECT 1 FROM lanes WHERE lane_id=NEW.lane_id AND thread_id IN (SELECT thread_id FROM thread_tombstones))
BEGIN SELECT RAISE(ABORT,'run or thread is tombstoned'); END;
