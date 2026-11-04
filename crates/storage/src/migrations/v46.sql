-- Phase 1c is opt-in. No backfill: old questions/reports do not gain notifications.
ALTER TABLE lanes ADD COLUMN wake_guard_json TEXT NOT NULL DEFAULT '{}';
ALTER TABLE lanes ADD COLUMN question_capacity_blocked_json TEXT;
ALTER TABLE lane_reports ADD COLUMN artifact_revision_json TEXT;
ALTER TABLE lane_reports ADD COLUMN failure_episode TEXT;
ALTER TABLE host_input_receipts ADD COLUMN provenance_json TEXT NOT NULL DEFAULT '{"kind":"user"}';

CREATE TABLE lane_question_routes (
 question_id TEXT PRIMARY KEY REFERENCES user_questions(id) ON DELETE CASCADE,
 lane_id TEXT NOT NULL REFERENCES lanes(lane_id) ON DELETE CASCADE,
 requester_run_id TEXT NOT NULL,
 requester_root_run_id TEXT NOT NULL,
 requester_thread_id TEXT NOT NULL,
 responder_thread_id TEXT,
 management_revision INTEGER NOT NULL CHECK(management_revision >= 1),
 routing_revision INTEGER NOT NULL CHECK(routing_revision >= 1),
 payload TEXT NOT NULL,
 operation_id TEXT,
 operation_json TEXT
);
CREATE INDEX lane_routes_responder ON lane_question_routes(responder_thread_id, question_id);
CREATE TABLE lane_notifications (
 notification_id TEXT PRIMARY KEY,
 lane_id TEXT NOT NULL REFERENCES lanes(lane_id) ON DELETE CASCADE,
 management_revision INTEGER NOT NULL CHECK(management_revision >= 1),
 event_kind TEXT NOT NULL,
 semantic_key TEXT NOT NULL,
 source_thread_id TEXT NOT NULL,
 destination_thread_id TEXT NOT NULL,
 payload TEXT NOT NULL CHECK(OCTET_LENGTH(payload) <= 4096),
 state TEXT NOT NULL DEFAULT 'pending' CHECK(state IN ('pending','acked','obsolete')),
 claim_epoch INTEGER,
 receipt_prompt_id TEXT UNIQUE,
 UNIQUE(lane_id, management_revision, event_kind, semantic_key)
);
CREATE INDEX lane_notifications_pending ON lane_notifications(destination_thread_id, state, notification_id);
CREATE TABLE lane_delivery_destinations (
 thread_id TEXT PRIMARY KEY,
 epoch INTEGER NOT NULL DEFAULT 0,
 stopped INTEGER NOT NULL DEFAULT 0 CHECK(stopped IN (0,1)),
 owner_json TEXT,
 claim_epoch INTEGER NOT NULL DEFAULT 0,
 lease_token TEXT,
 lease_until_ns INTEGER,
 last_nack TEXT
);
CREATE TRIGGER fence_lane_question_routes_insert BEFORE INSERT ON lane_question_routes
WHEN NEW.requester_thread_id IN (SELECT thread_id FROM thread_tombstones) OR NEW.responder_thread_id IN (SELECT thread_id FROM thread_tombstones) OR NEW.requester_run_id IN (SELECT run_id FROM run_tombstones) OR NEW.requester_root_run_id IN (SELECT run_id FROM run_tombstones) OR EXISTS (SELECT 1 FROM lanes WHERE lane_id=NEW.lane_id AND thread_id IN (SELECT thread_id FROM thread_tombstones))
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
CREATE TRIGGER fence_lane_question_routes_update BEFORE UPDATE ON lane_question_routes
WHEN NEW.requester_thread_id IN (SELECT thread_id FROM thread_tombstones) OR NEW.responder_thread_id IN (SELECT thread_id FROM thread_tombstones) OR NEW.requester_run_id IN (SELECT run_id FROM run_tombstones) OR NEW.requester_root_run_id IN (SELECT run_id FROM run_tombstones) OR EXISTS (SELECT 1 FROM lanes WHERE lane_id=NEW.lane_id AND thread_id IN (SELECT thread_id FROM thread_tombstones))
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
CREATE TRIGGER fence_lane_notifications_insert BEFORE INSERT ON lane_notifications
WHEN NEW.source_thread_id IN (SELECT thread_id FROM thread_tombstones) OR NEW.destination_thread_id IN (SELECT thread_id FROM thread_tombstones) OR EXISTS (SELECT 1 FROM lanes WHERE lane_id=NEW.lane_id AND thread_id IN (SELECT thread_id FROM thread_tombstones))
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
CREATE TRIGGER fence_lane_notifications_update BEFORE UPDATE ON lane_notifications
WHEN NEW.source_thread_id IN (SELECT thread_id FROM thread_tombstones) OR NEW.destination_thread_id IN (SELECT thread_id FROM thread_tombstones) OR EXISTS (SELECT 1 FROM lanes WHERE lane_id=NEW.lane_id AND thread_id IN (SELECT thread_id FROM thread_tombstones))
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
CREATE TRIGGER fence_lane_delivery_destinations_insert BEFORE INSERT ON lane_delivery_destinations
WHEN NEW.thread_id IN (SELECT thread_id FROM thread_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
CREATE TRIGGER fence_lane_delivery_destinations_update BEFORE UPDATE ON lane_delivery_destinations
WHEN NEW.thread_id IN (SELECT thread_id FROM thread_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
