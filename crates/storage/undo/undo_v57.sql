-- Test-only reconstruction of v56, never a production downgrade.
DROP TRIGGER lane_close_immutable;
DROP TRIGGER fence_lane_closes_insert;
DROP TRIGGER fence_lane_closes_update;
DROP INDEX lane_closes_lane;
ALTER TABLE lane_closes RENAME TO lane_closes_v57;
CREATE TABLE lane_closes (
 close_id TEXT PRIMARY KEY,
 lane_id TEXT NOT NULL REFERENCES lanes(lane_id) ON DELETE CASCADE,
 verification_id TEXT NOT NULL,
 payload TEXT NOT NULL CHECK(OCTET_LENGTH(payload) <= 4096),
 pr_evidence_json TEXT CHECK(OCTET_LENGTH(pr_evidence_json) <= 4096)
);
INSERT INTO lane_closes(close_id,lane_id,verification_id,payload)
SELECT close_id,lane_id,COALESCE(verification_id,''),payload FROM lane_closes_v57 WHERE OCTET_LENGTH(payload) <= 4096;
DROP TABLE lane_closes_v57;
CREATE INDEX lane_closes_lane ON lane_closes(lane_id);
CREATE TRIGGER lane_close_immutable BEFORE UPDATE ON lane_closes
BEGIN SELECT RAISE(ABORT,'lane close history is immutable'); END;
CREATE TRIGGER fence_lane_closes_insert BEFORE INSERT ON lane_closes
WHEN EXISTS(SELECT 1 FROM lanes WHERE lane_id=NEW.lane_id AND thread_id IN (SELECT thread_id FROM thread_tombstones))
BEGIN SELECT RAISE(ABORT,'run or thread is tombstoned'); END;
CREATE TRIGGER fence_lane_closes_update BEFORE UPDATE ON lane_closes
WHEN EXISTS(SELECT 1 FROM lanes WHERE lane_id=NEW.lane_id AND thread_id IN (SELECT thread_id FROM thread_tombstones))
BEGIN SELECT RAISE(ABORT,'run or thread is tombstoned'); END;
