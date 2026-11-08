-- Test-only old-schema reconstruction.
DROP INDEX idx_lanes_retention_root;
DROP TRIGGER lane_membership_epoch_insert;
DROP TRIGGER lane_management_list_invalidation;
DROP TRIGGER fence_lane_management_receipts_insert;
DROP TRIGGER lane_management_receipt_immutable;
DROP TABLE lane_management_receipts;
ALTER TABLE retention_staging_state DROP COLUMN family_topology_revision;
ALTER TABLE lane_state DROP COLUMN family_topology_revision;
ALTER TABLE lanes DROP COLUMN management_projection_revision;
ALTER TABLE lanes DROP COLUMN detached_since_management_revision;

DROP TRIGGER fence_lane_notifications_update;
CREATE TRIGGER fence_lane_notifications_update BEFORE UPDATE ON lane_notifications
WHEN NEW.source_thread_id IN (SELECT thread_id FROM thread_tombstones) OR NEW.destination_thread_id IN (SELECT thread_id FROM thread_tombstones) OR EXISTS (SELECT 1 FROM lanes WHERE lane_id=NEW.lane_id AND thread_id IN (SELECT thread_id FROM thread_tombstones))
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;

DROP TRIGGER fence_threads_insert;
CREATE TRIGGER fence_threads_insert BEFORE INSERT ON threads
WHEN NEW.thread_id IN (SELECT thread_id FROM thread_tombstones) OR NEW.parent_thread_id IN (SELECT thread_id FROM thread_tombstones) OR NEW.escalation_source_run_id IN (SELECT run_id FROM run_tombstones) OR NEW.root_run_id IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;

DROP TRIGGER fence_threads_update;
CREATE TRIGGER fence_threads_update BEFORE UPDATE ON threads
WHEN NEW.thread_id IN (SELECT thread_id FROM thread_tombstones) OR NEW.parent_thread_id IN (SELECT thread_id FROM thread_tombstones) OR NEW.escalation_source_run_id IN (SELECT run_id FROM run_tombstones) OR NEW.root_run_id IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
