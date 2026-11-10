DROP TRIGGER fence_lane_notifications_update;
CREATE TRIGGER fence_lane_notifications_update BEFORE UPDATE ON lane_notifications
WHEN (NEW.source_thread_id IN (SELECT thread_id FROM thread_tombstones)
 AND NOT (OLD.source_kind='lane' AND NEW.source_kind='lane'
  AND OLD.event_kind='answer' AND NEW.event_kind='answer'
  AND NEW.notification_id=OLD.notification_id AND NEW.lane_id=OLD.lane_id
  AND NEW.source_thread_id=OLD.source_thread_id AND NEW.destination_thread_id=OLD.destination_thread_id
  AND NEW.semantic_key=OLD.semantic_key AND NEW.pr_key IS OLD.pr_key))
 OR NEW.destination_thread_id IN (SELECT thread_id FROM thread_tombstones)
 OR EXISTS (SELECT 1 FROM lanes WHERE lane_id=NEW.lane_id AND thread_id IN (SELECT thread_id FROM thread_tombstones))
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
DROP TRIGGER fence_lane_instruction_receipts_insert;
DROP TRIGGER fence_lane_instructions_insert;
DROP TRIGGER lane_instruction_receipt_immutable;
DROP TRIGGER lane_instruction_immutable;
DROP TABLE lane_instruction_receipts;
DROP TABLE lane_instructions;
