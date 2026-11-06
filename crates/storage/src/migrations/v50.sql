-- Phase 3A: management receipts, membership epochs and repair fences.
ALTER TABLE lanes ADD COLUMN detached_since_management_revision INTEGER CHECK(detached_since_management_revision >= 1);
UPDATE lanes SET detached_since_management_revision=management_revision WHERE policy='detached';
ALTER TABLE lanes ADD COLUMN management_projection_revision INTEGER NOT NULL DEFAULT 0 CHECK(management_projection_revision >= 0);
ALTER TABLE lane_state ADD COLUMN family_topology_revision INTEGER NOT NULL DEFAULT 0 CHECK(family_topology_revision >= 0);
-- Consumed by Phase 3C's staging invalidation; no rebuild algorithm here.
ALTER TABLE retention_staging_state ADD COLUMN family_topology_revision INTEGER NOT NULL DEFAULT 0 CHECK(family_topology_revision >= 0);
CREATE TABLE lane_management_receipts (
 operation_id TEXT PRIMARY KEY CHECK(OCTET_LENGTH(operation_id) BETWEEN 1 AND 256),
 lane_id TEXT NOT NULL REFERENCES lanes(lane_id) ON DELETE CASCADE,
 request_json TEXT NOT NULL,
 actor_json TEXT NOT NULL,
 result_json TEXT NOT NULL,
 created_at_ns INTEGER NOT NULL
);
CREATE INDEX lane_management_receipts_lane ON lane_management_receipts(lane_id);
CREATE TRIGGER lane_management_receipt_immutable BEFORE UPDATE ON lane_management_receipts
BEGIN SELECT RAISE(ABORT,'management receipt is immutable'); END;
CREATE TRIGGER fence_lane_management_receipts_insert BEFORE INSERT ON lane_management_receipts
WHEN EXISTS(SELECT 1 FROM lanes WHERE lane_id=NEW.lane_id AND thread_id IN (SELECT thread_id FROM thread_tombstones))
BEGIN SELECT RAISE(ABORT,'run or thread is tombstoned'); END;
CREATE TRIGGER lane_membership_epoch_insert AFTER INSERT ON lanes WHEN NEW.policy='detached'
BEGIN UPDATE lanes SET detached_since_management_revision=NEW.management_revision WHERE lane_id=NEW.lane_id; END;
-- Lists freeze order, not authority. Every management edit invalidates old cursors,
-- including a filtered member leaving the list. Ordinary reports do not.
CREATE TRIGGER lane_management_list_invalidation AFTER UPDATE OF management_revision ON lanes
WHEN NEW.management_revision<>OLD.management_revision
BEGIN DELETE FROM lane_list_snapshots; END;
-- An episode keeps its identity and one-shot slot through a lead-preserving policy
-- edit. Only its management fence may be advanced; never its progress anchor.
DROP TRIGGER lane_episode_identity;
CREATE TRIGGER lane_episode_identity BEFORE UPDATE ON lane_stall_episodes
WHEN NEW.episode_id<>OLD.episode_id OR NEW.lane_id<>OLD.lane_id
 OR NEW.management_revision<OLD.management_revision OR NEW.progress_epoch<>OLD.progress_epoch
 OR json_extract(NEW.payload,'$.progress_anchor')<>json_extract(OLD.payload,'$.progress_anchor')
 OR (json_extract(OLD.payload,'$.resolved_at_ns') IS NOT NULL AND NEW.payload<>OLD.payload)
 OR (json_extract(OLD.payload,'$.nudge_state')<>'not_requested' AND json_extract(NEW.payload,'$.nudge_state')='not_requested')
BEGIN SELECT RAISE(ABORT,'episode identity and consumed nudge slot are immutable'); END;

-- A live handoff recipient keeps the question independently of its requester.
-- Insert stays strictly fenced; only a retained question may receive an answer.
DROP TRIGGER fence_user_questions_update;
CREATE TRIGGER fence_user_questions_update BEFORE UPDATE ON user_questions
WHEN (NEW.run_id IN (SELECT run_id FROM run_tombstones) OR NEW.root_run_id IN (SELECT run_id FROM run_tombstones))
 AND NOT EXISTS (SELECT 1 FROM user_question_links l WHERE l.question_id=NEW.id AND l.run_id NOT IN (SELECT run_id FROM run_tombstones))
BEGIN SELECT RAISE(ABORT,'run or thread is tombstoned'); END;

-- Accepted Answer source/responder IDs are provenance, not ownership edges.
-- Only existing Answers with unchanged identity can be ACKed/re-stamped after
-- their source is reclaimed. Inserts and live destinations/lanes stay fenced.
DROP TRIGGER fence_lane_notifications_update;
CREATE TRIGGER fence_lane_notifications_update BEFORE UPDATE ON lane_notifications
WHEN (NEW.source_thread_id IN (SELECT thread_id FROM thread_tombstones)
 AND NOT (OLD.event_kind='answer' AND NEW.event_kind='answer'
  AND NEW.notification_id=OLD.notification_id AND NEW.lane_id=OLD.lane_id
  AND NEW.source_thread_id=OLD.source_thread_id AND NEW.destination_thread_id=OLD.destination_thread_id))
 OR NEW.destination_thread_id IN (SELECT thread_id FROM thread_tombstones)
 OR EXISTS (SELECT 1 FROM lanes WHERE lane_id=NEW.lane_id AND thread_id IN (SELECT thread_id FROM thread_tombstones))
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
-- UPSERT fires INSERT fences before conflict UPDATE. Repeating an existing
-- lane's source is not assigning a new dead source.
DROP TRIGGER fence_threads_insert;
CREATE TRIGGER fence_threads_insert BEFORE INSERT ON threads
WHEN NEW.thread_id IN (SELECT thread_id FROM thread_tombstones)
 OR NEW.parent_thread_id IN (SELECT thread_id FROM thread_tombstones)
 OR (NEW.escalation_source_run_id IN (SELECT run_id FROM run_tombstones)
  AND NOT EXISTS(SELECT 1 FROM threads t JOIN lanes l ON l.thread_id=t.thread_id
   WHERE t.thread_id=NEW.thread_id AND t.escalation_source_run_id=NEW.escalation_source_run_id))
 OR NEW.root_run_id IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
-- Handoff may retain escalation_source_run_id as provenance on an existing lane.
-- It never permits assigning a new dead source, or updating a deleted owner/root.
DROP TRIGGER fence_threads_update;
CREATE TRIGGER fence_threads_update BEFORE UPDATE ON threads
WHEN NEW.thread_id IN (SELECT thread_id FROM thread_tombstones)
 OR NEW.parent_thread_id IN (SELECT thread_id FROM thread_tombstones)
 OR (NEW.escalation_source_run_id IN (SELECT run_id FROM run_tombstones)
  AND NOT (NEW.escalation_source_run_id IS OLD.escalation_source_run_id
   AND EXISTS(SELECT 1 FROM lanes l WHERE l.thread_id=NEW.thread_id)))
 OR NEW.root_run_id IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
