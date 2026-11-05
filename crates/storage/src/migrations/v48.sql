-- ADR 0044 Phase 2: request invalidation and nudge progress are distinct.
ALTER TABLE lanes ADD COLUMN request_revision INTEGER NOT NULL DEFAULT 0 CHECK(request_revision >= 0);
ALTER TABLE lanes ADD COLUMN progress_epoch INTEGER NOT NULL DEFAULT 0 CHECK(progress_epoch >= 0);
ALTER TABLE lanes ADD COLUMN current_close_id TEXT;
CREATE TABLE lane_stall_episodes (
 episode_id TEXT PRIMARY KEY,
 lane_id TEXT NOT NULL REFERENCES lanes(lane_id) ON DELETE CASCADE,
 management_revision INTEGER NOT NULL,
 progress_epoch INTEGER NOT NULL,
 payload TEXT NOT NULL CHECK(OCTET_LENGTH(payload) <= 4096),
 UNIQUE(lane_id,management_revision,progress_epoch)
);
CREATE TABLE lane_verifications (
 verification_id TEXT PRIMARY KEY,
 lane_id TEXT NOT NULL REFERENCES lanes(lane_id) ON DELETE CASCADE,
 reviewer_run_id TEXT NOT NULL UNIQUE,
 payload TEXT NOT NULL CHECK(OCTET_LENGTH(payload) <= 32768)
);
CREATE INDEX lane_verifications_lane ON lane_verifications(lane_id);
CREATE TABLE lane_closes (
 close_id TEXT PRIMARY KEY,
 lane_id TEXT NOT NULL REFERENCES lanes(lane_id) ON DELETE CASCADE,
 verification_id TEXT NOT NULL,
 payload TEXT NOT NULL CHECK(OCTET_LENGTH(payload) <= 4096),
 pr_evidence_json TEXT CHECK(OCTET_LENGTH(pr_evidence_json) <= 4096)
);
CREATE INDEX lane_closes_lane ON lane_closes(lane_id);
CREATE TABLE lane_forward_receipts (
 caller_root TEXT NOT NULL,
 operation_id TEXT NOT NULL,
 lane_id TEXT NOT NULL REFERENCES lanes(lane_id) ON DELETE CASCADE,
 question_id TEXT NOT NULL REFERENCES user_questions(id) ON DELETE CASCADE,
 request_json TEXT NOT NULL,
 result_json TEXT NOT NULL,
 PRIMARY KEY(caller_root,operation_id)
);
CREATE INDEX lane_forward_receipts_lane ON lane_forward_receipts(lane_id);
CREATE TRIGGER lane_brief_invalidation AFTER UPDATE OF brief_revision ON lanes
WHEN NEW.brief_revision <> OLD.brief_revision
BEGIN UPDATE lanes SET request_revision=request_revision+1,current_close_id=NULL WHERE lane_id=NEW.lane_id; END;
CREATE TRIGGER lane_management_invalidation AFTER UPDATE OF management_revision ON lanes
WHEN NEW.management_revision <> OLD.management_revision
BEGIN UPDATE lanes SET current_close_id=NULL WHERE lane_id=NEW.lane_id; END;
CREATE TRIGGER lane_phase2_monotonic BEFORE UPDATE ON lanes
WHEN NEW.request_revision < OLD.request_revision OR NEW.progress_epoch < OLD.progress_epoch
BEGIN SELECT RAISE(ABORT,'lane counters never go back'); END;
CREATE TRIGGER lane_close_immutable BEFORE UPDATE ON lane_closes
BEGIN SELECT RAISE(ABORT,'lane close history is immutable'); END;
CREATE TRIGGER lane_forward_immutable BEFORE UPDATE ON lane_forward_receipts
BEGIN SELECT RAISE(ABORT,'forward receipts are immutable'); END;
CREATE TRIGGER fence_lane_stall_episodes_insert BEFORE INSERT ON lane_stall_episodes
WHEN EXISTS(SELECT 1 FROM lanes WHERE lane_id=NEW.lane_id AND thread_id IN (SELECT thread_id FROM thread_tombstones))
BEGIN SELECT RAISE(ABORT,'run or thread is tombstoned'); END;
CREATE TRIGGER fence_lane_stall_episodes_update BEFORE UPDATE ON lane_stall_episodes
WHEN EXISTS(SELECT 1 FROM lanes WHERE lane_id=NEW.lane_id AND thread_id IN (SELECT thread_id FROM thread_tombstones))
BEGIN SELECT RAISE(ABORT,'run or thread is tombstoned'); END;
CREATE TRIGGER fence_lane_verifications_insert BEFORE INSERT ON lane_verifications
WHEN EXISTS(SELECT 1 FROM lanes WHERE lane_id=NEW.lane_id AND thread_id IN (SELECT thread_id FROM thread_tombstones)) OR NEW.reviewer_run_id IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT,'run or thread is tombstoned'); END;
CREATE TRIGGER fence_lane_verifications_update BEFORE UPDATE ON lane_verifications
WHEN EXISTS(SELECT 1 FROM lanes WHERE lane_id=NEW.lane_id AND thread_id IN (SELECT thread_id FROM thread_tombstones)) OR NEW.reviewer_run_id IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT,'run or thread is tombstoned'); END;
CREATE TRIGGER fence_lane_closes_insert BEFORE INSERT ON lane_closes
WHEN EXISTS(SELECT 1 FROM lanes WHERE lane_id=NEW.lane_id AND thread_id IN (SELECT thread_id FROM thread_tombstones))
BEGIN SELECT RAISE(ABORT,'run or thread is tombstoned'); END;
CREATE TRIGGER fence_lane_closes_update BEFORE UPDATE ON lane_closes
WHEN EXISTS(SELECT 1 FROM lanes WHERE lane_id=NEW.lane_id AND thread_id IN (SELECT thread_id FROM thread_tombstones))
BEGIN SELECT RAISE(ABORT,'run or thread is tombstoned'); END;
CREATE TRIGGER fence_lane_forward_receipts_insert BEFORE INSERT ON lane_forward_receipts
WHEN EXISTS(SELECT 1 FROM lanes WHERE lane_id=NEW.lane_id AND thread_id IN (SELECT thread_id FROM thread_tombstones)) OR NEW.caller_root IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT,'run or thread is tombstoned'); END;
CREATE TRIGGER fence_lane_forward_receipts_update BEFORE UPDATE ON lane_forward_receipts
WHEN EXISTS(SELECT 1 FROM lanes WHERE lane_id=NEW.lane_id AND thread_id IN (SELECT thread_id FROM thread_tombstones)) OR NEW.caller_root IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT,'run or thread is tombstoned'); END;

-- Defend the immutable evidence/episode identities even from a stale SQL writer.
CREATE TRIGGER lane_episode_identity BEFORE UPDATE ON lane_stall_episodes
WHEN NEW.episode_id<>OLD.episode_id OR NEW.lane_id<>OLD.lane_id
 OR NEW.management_revision<>OLD.management_revision OR NEW.progress_epoch<>OLD.progress_epoch
 OR json_extract(NEW.payload,'$.progress_anchor')<>json_extract(OLD.payload,'$.progress_anchor')
 OR (json_extract(OLD.payload,'$.resolved_at_ns') IS NOT NULL AND NEW.payload<>OLD.payload)
 OR (json_extract(OLD.payload,'$.nudge_state')<>'not_requested' AND json_extract(NEW.payload,'$.nudge_state')='not_requested')
BEGIN SELECT RAISE(ABORT,'episode identity and consumed nudge slot are immutable'); END;
CREATE TRIGGER lane_verification_identity BEFORE UPDATE ON lane_verifications
WHEN NEW.verification_id<>OLD.verification_id OR NEW.lane_id<>OLD.lane_id OR NEW.reviewer_run_id<>OLD.reviewer_run_id
 OR json_extract(NEW.payload,'$.target')<>json_extract(OLD.payload,'$.target')
 OR json_extract(NEW.payload,'$.pr_evidence') IS NOT json_extract(OLD.payload,'$.pr_evidence')
 OR (json_extract(OLD.payload,'$.submission') IS NOT NULL AND json_extract(NEW.payload,'$.submission') IS NOT json_extract(OLD.payload,'$.submission'))
 OR (json_extract(OLD.payload,'$.state')<>'pending' AND json_extract(NEW.payload,'$.state')='pending')
BEGIN SELECT RAISE(ABORT,'verification snapshot and submitted evidence are immutable'); END;
