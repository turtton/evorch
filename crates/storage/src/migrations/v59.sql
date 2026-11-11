-- A lead's supplement or brief revision for a lane. The row is the immutable, fully
-- presented text the lane conversation receives; the outbox row only references it.
CREATE TABLE lane_instructions (
 lane_id TEXT NOT NULL REFERENCES lanes(lane_id) ON DELETE CASCADE,
 instruction_seq INTEGER NOT NULL CHECK(instruction_seq >= 1),
 kind TEXT NOT NULL CHECK(kind IN ('supplement','brief_revision')),
 operation_id TEXT NOT NULL UNIQUE CHECK(OCTET_LENGTH(operation_id) BETWEEN 1 AND 256),
 body TEXT NOT NULL CHECK(OCTET_LENGTH(body) <= 32768),
 lead_thread_id TEXT NOT NULL,
 lead_root_run_id TEXT NOT NULL,
 accepted_management_revision INTEGER NOT NULL CHECK(accepted_management_revision >= 1),
 brief_revision_before INTEGER NOT NULL CHECK(brief_revision_before >= 1),
 brief_revision_after INTEGER NOT NULL CHECK(brief_revision_after >= 1),
 notification_id TEXT NOT NULL UNIQUE,
 created_at_ns INTEGER NOT NULL,
 PRIMARY KEY(lane_id, instruction_seq)
);
-- Every operation, including a brief revision that changed nothing, leaves a receipt
-- so that a retry after a lost reply returns the original outcome.
CREATE TABLE lane_instruction_receipts (
 operation_id TEXT PRIMARY KEY CHECK(OCTET_LENGTH(operation_id) BETWEEN 1 AND 256),
 lane_id TEXT NOT NULL REFERENCES lanes(lane_id) ON DELETE CASCADE,
 request_json TEXT NOT NULL,
 actor_json TEXT NOT NULL,
 result_json TEXT NOT NULL,
 created_at_ns INTEGER NOT NULL
);
CREATE INDEX lane_instruction_receipts_lane ON lane_instruction_receipts(lane_id);
CREATE TRIGGER lane_instruction_immutable BEFORE UPDATE ON lane_instructions
BEGIN SELECT RAISE(ABORT,'lane instruction is immutable'); END;
CREATE TRIGGER lane_instruction_receipt_immutable BEFORE UPDATE ON lane_instruction_receipts
BEGIN SELECT RAISE(ABORT,'lane instruction receipt is immutable'); END;
CREATE TRIGGER fence_lane_instructions_insert BEFORE INSERT ON lane_instructions
WHEN EXISTS(SELECT 1 FROM lanes WHERE lane_id=NEW.lane_id AND thread_id IN (SELECT thread_id FROM thread_tombstones))
BEGIN SELECT RAISE(ABORT,'run or thread is tombstoned'); END;
CREATE TRIGGER fence_lane_instruction_receipts_insert BEFORE INSERT ON lane_instruction_receipts
WHEN EXISTS(SELECT 1 FROM lanes WHERE lane_id=NEW.lane_id AND thread_id IN (SELECT thread_id FROM thread_tombstones))
BEGIN SELECT RAISE(ABORT,'run or thread is tombstoned'); END;

-- An accepted instruction, like an accepted Answer, keeps its sender only as
-- provenance: the lead thread may be deleted while the lane still has to receive it.
DROP TRIGGER fence_lane_notifications_update;
CREATE TRIGGER fence_lane_notifications_update BEFORE UPDATE ON lane_notifications
WHEN (NEW.source_thread_id IN (SELECT thread_id FROM thread_tombstones)
 AND NOT (OLD.source_kind='lane' AND NEW.source_kind='lane'
  AND OLD.event_kind IN ('answer','instruction') AND NEW.event_kind=OLD.event_kind
  AND NEW.notification_id=OLD.notification_id AND NEW.lane_id=OLD.lane_id
  AND NEW.source_thread_id=OLD.source_thread_id AND NEW.destination_thread_id=OLD.destination_thread_id
  AND NEW.semantic_key=OLD.semantic_key AND NEW.pr_key IS OLD.pr_key))
 OR NEW.destination_thread_id IN (SELECT thread_id FROM thread_tombstones)
 OR EXISTS (SELECT 1 FROM lanes WHERE lane_id=NEW.lane_id AND thread_id IN (SELECT thread_id FROM thread_tombstones))
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
