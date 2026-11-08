-- Generalize the existing outbox, preserving lane semantic IDs and wire payloads.
-- The historical table names remain; a PR never creates a lane row.
DROP TRIGGER fence_lane_notifications_insert;
DROP TRIGGER fence_lane_notifications_update;
DROP INDEX lane_notifications_pending;
ALTER TABLE lane_notifications RENAME TO old_lane_notifications;
CREATE TABLE lane_notifications (
 notification_id TEXT PRIMARY KEY,
 source_kind TEXT NOT NULL DEFAULT 'lane' CHECK(source_kind IN ('lane','pull_request_checks_failed')),
 lane_id TEXT REFERENCES lanes(lane_id) ON DELETE CASCADE,
 pr_key TEXT,
 source_key TEXT GENERATED ALWAYS AS (CASE source_kind WHEN 'lane' THEN lane_id ELSE pr_key END) STORED,
 management_revision INTEGER NOT NULL CHECK(management_revision >= 1),
 event_kind TEXT NOT NULL,
 semantic_key TEXT NOT NULL,
 source_thread_id TEXT NOT NULL,
 destination_thread_id TEXT NOT NULL,
 payload TEXT NOT NULL CHECK(OCTET_LENGTH(payload) <= 4096),
 state TEXT NOT NULL DEFAULT 'pending' CHECK(state IN ('pending','acked','obsolete')),
 claim_epoch INTEGER,
 receipt_prompt_id TEXT UNIQUE,
 CHECK((source_kind='lane' AND lane_id IS NOT NULL AND pr_key IS NULL) OR
       (source_kind='pull_request_checks_failed' AND lane_id IS NULL AND pr_key IS NOT NULL)),
 UNIQUE(source_kind,source_key,management_revision,event_kind,semantic_key)
);
INSERT INTO lane_notifications(notification_id,lane_id,management_revision,event_kind,semantic_key,source_thread_id,destination_thread_id,payload,state,claim_epoch,receipt_prompt_id)
 SELECT notification_id,lane_id,management_revision,event_kind,semantic_key,source_thread_id,destination_thread_id,payload,state,claim_epoch,receipt_prompt_id FROM old_lane_notifications;
DROP TABLE old_lane_notifications;
CREATE INDEX lane_notifications_pending ON lane_notifications(destination_thread_id,source_kind,state,notification_id);
CREATE TABLE pr_auto_fix_state (
 pr_key TEXT PRIMARY KEY,
 enabled INTEGER NOT NULL DEFAULT 0 CHECK(enabled IN (0,1)),
 destination_thread_id TEXT,
 link_revision INTEGER,
 policy_revision INTEGER NOT NULL DEFAULT 0,
 budget_epoch INTEGER NOT NULL DEFAULT 0,
 used INTEGER NOT NULL DEFAULT 0 CHECK(used BETWEEN 0 AND 3),
 cooldown_until_ns INTEGER,
 stop_reason TEXT,
 limit_reached_epoch INTEGER,
 wake_guard_json TEXT NOT NULL DEFAULT '{}'
);
CREATE TABLE pr_notices (
 notice_id TEXT PRIMARY KEY,
 pr_key TEXT NOT NULL REFERENCES pr_auto_fix_state(pr_key),
 head_sha TEXT NOT NULL,
 mode TEXT NOT NULL CHECK(mode IN ('manual','auto')),
 destination_thread_id TEXT,
 link_revision INTEGER NOT NULL,
 policy_revision INTEGER NOT NULL,
 budget_epoch INTEGER NOT NULL,
 success_seq INTEGER NOT NULL,
 phase TEXT NOT NULL CHECK(phase IN ('reserved','ready','adopted','cancelled')),
 payload TEXT CHECK(OCTET_LENGTH(payload)<=16384),
 UNIQUE(pr_key,head_sha)
);
CREATE INDEX pr_notices_destination ON pr_notices(destination_thread_id,phase);
CREATE INDEX pr_notices_budget ON pr_notices(pr_key,budget_epoch,mode,phase);
-- One durable event per exhausted budget. Consumers may project it repeatedly,
-- but cannot record a second exhaustion event for the same epoch.
CREATE TABLE pr_auto_fix_limit_events (
 pr_key TEXT NOT NULL REFERENCES pr_auto_fix_state(pr_key),
 budget_epoch INTEGER NOT NULL,
 notice_id TEXT NOT NULL,
 at_ns INTEGER NOT NULL,
 PRIMARY KEY(pr_key,budget_epoch)
);
CREATE TRIGGER fence_lane_notifications_insert BEFORE INSERT ON lane_notifications
WHEN NEW.source_thread_id IN (SELECT thread_id FROM thread_tombstones) OR NEW.destination_thread_id IN (SELECT thread_id FROM thread_tombstones) OR EXISTS (SELECT 1 FROM lanes WHERE lane_id=NEW.lane_id AND thread_id IN (SELECT thread_id FROM thread_tombstones))
BEGIN SELECT RAISE(ABORT,'run or thread is tombstoned'); END;
CREATE TRIGGER fence_lane_notifications_update BEFORE UPDATE ON lane_notifications
WHEN NEW.source_thread_id IN (SELECT thread_id FROM thread_tombstones) OR NEW.destination_thread_id IN (SELECT thread_id FROM thread_tombstones) OR EXISTS (SELECT 1 FROM lanes WHERE lane_id=NEW.lane_id AND thread_id IN (SELECT thread_id FROM thread_tombstones))
BEGIN SELECT RAISE(ABORT,'run or thread is tombstoned'); END;
CREATE TRIGGER fence_pr_notices_insert BEFORE INSERT ON pr_notices
WHEN NEW.destination_thread_id IN (SELECT thread_id FROM thread_tombstones)
BEGIN SELECT RAISE(ABORT,'run or thread is tombstoned'); END;
CREATE TRIGGER fence_pr_notices_update BEFORE UPDATE ON pr_notices
WHEN NEW.destination_thread_id IN (SELECT thread_id FROM thread_tombstones)
BEGIN SELECT RAISE(ABORT,'run or thread is tombstoned'); END;
CREATE TRIGGER fence_pr_policy_insert BEFORE INSERT ON pr_auto_fix_state
WHEN NEW.destination_thread_id IN (SELECT thread_id FROM thread_tombstones)
BEGIN SELECT RAISE(ABORT,'run or thread is tombstoned'); END;
CREATE TRIGGER fence_pr_policy_update BEFORE UPDATE ON pr_auto_fix_state
WHEN NEW.destination_thread_id IN (SELECT thread_id FROM thread_tombstones)
BEGIN SELECT RAISE(ABORT,'run or thread is tombstoned'); END;
-- Lifecycle invalidation is transactional with the original operation. No fallback
-- destination is selected, and unarchive/relink cannot implicitly resume policy.
CREATE TRIGGER pr_policy_link_ended AFTER UPDATE OF superseded_at_ns ON thread_pull_requests
WHEN NEW.superseded_at_ns IS NOT NULL AND OLD.superseded_at_ns IS NULL
BEGIN
 UPDATE pr_auto_fix_state SET enabled=0,stop_reason='unlinked',policy_revision=policy_revision+1,destination_thread_id=NULL,link_revision=NULL WHERE destination_thread_id=NEW.thread_id;
 UPDATE pr_notices SET phase='cancelled',payload=NULL WHERE destination_thread_id=NEW.thread_id AND phase IN ('reserved','ready');
 UPDATE lane_notifications SET state='obsolete' WHERE destination_thread_id=NEW.thread_id AND source_kind='pull_request_checks_failed' AND receipt_prompt_id IS NULL;
END;
CREATE TRIGGER pr_policy_archived AFTER UPDATE OF archived ON threads WHEN NEW.archived=1 AND OLD.archived=0
BEGIN
 UPDATE pr_auto_fix_state SET enabled=0,stop_reason='archived',policy_revision=policy_revision+1,destination_thread_id=NULL,link_revision=NULL WHERE destination_thread_id=NEW.thread_id;
 UPDATE pr_notices SET phase='cancelled',payload=NULL WHERE destination_thread_id=NEW.thread_id AND phase IN ('reserved','ready');
 UPDATE lane_notifications SET state='obsolete' WHERE destination_thread_id=NEW.thread_id AND source_kind='pull_request_checks_failed' AND receipt_prompt_id IS NULL;
END;
CREATE TRIGGER pr_policy_archived_insert AFTER INSERT ON threads WHEN NEW.archived=1
BEGIN
 UPDATE pr_auto_fix_state SET enabled=0,stop_reason='archived',policy_revision=policy_revision+1,destination_thread_id=NULL,link_revision=NULL WHERE destination_thread_id=NEW.thread_id;
 UPDATE pr_notices SET phase='cancelled',payload=NULL WHERE destination_thread_id=NEW.thread_id AND phase IN ('reserved','ready');
 UPDATE lane_notifications SET state='obsolete' WHERE destination_thread_id=NEW.thread_id AND source_kind='pull_request_checks_failed' AND receipt_prompt_id IS NULL;
END;
CREATE TRIGGER pr_policy_deleted AFTER INSERT ON thread_tombstones
BEGIN
 UPDATE pr_auto_fix_state SET enabled=0,stop_reason='deleted',policy_revision=policy_revision+1,destination_thread_id=NULL,link_revision=NULL WHERE destination_thread_id=NEW.thread_id;
 -- Keep only the PR/SHA dedup identity for adopted notices. Cancels are reusable.
 UPDATE pr_notices SET phase=CASE phase WHEN 'adopted' THEN phase ELSE 'cancelled' END,payload=NULL,destination_thread_id=NULL,link_revision=0,policy_revision=0,success_seq=0 WHERE destination_thread_id=NEW.thread_id;
END;
