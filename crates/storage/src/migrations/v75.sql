-- Independent activity/goal projection. No payload scan in migration.
CREATE INDEX idx_retention_questions_root_pending ON user_questions(root_run_id,answered);
CREATE TABLE retention_activity_control(id INTEGER PRIMARY KEY CHECK(id=1), revision INTEGER NOT NULL DEFAULT 0, binding_revision INTEGER NOT NULL DEFAULT 0, decision_revision INTEGER NOT NULL DEFAULT 0, legacy_context_rowid INTEGER NOT NULL, legacy_binding_rowid INTEGER NOT NULL);
INSERT INTO retention_activity_control SELECT 1,0,0,0,coalesce((SELECT max(rowid) FROM run_contexts),0),coalesce((SELECT max(rowid) FROM thread_runs),0);
CREATE TABLE retention_activity_cursors(stream INTEGER PRIMARY KEY, kind TEXT, subkind TEXT, after_id INTEGER NOT NULL DEFAULT 0, high_water INTEGER NOT NULL DEFAULT 0, complete INTEGER NOT NULL DEFAULT 0);
INSERT INTO retention_activity_cursors(stream,kind,subkind) VALUES(0,'Message','MessageCompleted');
INSERT INTO retention_activity_cursors(stream,kind,subkind) VALUES(1,'Message','FinalResultPublished');
INSERT INTO retention_activity_cursors(stream,kind,subkind) VALUES(2,'Message','MessagePart');
INSERT INTO retention_activity_cursors(stream,kind,subkind) VALUES(3,'Lifecycle','AgentRunStateChanged');
INSERT INTO retention_activity_cursors(stream,kind,subkind) VALUES(4,'Lifecycle','TurnCompleted');
INSERT INTO retention_activity_cursors(stream,kind,subkind) VALUES(5,'Orchestrator','ThreadGoalUpdated');
INSERT INTO retention_activity_cursors(stream,kind,subkind) VALUES(6,'AgentMessage','Delivered');
INSERT INTO retention_activity_cursors(stream,kind,subkind) VALUES(7,'Message',NULL);
INSERT INTO retention_activity_cursors(stream,kind,subkind) VALUES(8,'Lifecycle',NULL);
INSERT INTO retention_activity_cursors(stream,kind,subkind) VALUES(9,'Tool',NULL);
INSERT INTO retention_activity_cursors(stream,kind,subkind) VALUES(10,'Provider',NULL);
INSERT INTO retention_activity_cursors(stream,kind,subkind) VALUES(11,'Orchestrator',NULL);
INSERT INTO retention_activity_cursors(stream,kind,subkind) VALUES(12,'AgentMessage',NULL);
INSERT INTO retention_activity_cursors(stream,kind,subkind) VALUES(13,'Usage',NULL);
INSERT INTO retention_activity_cursors(stream,kind,subkind) VALUES(14,'Fault',NULL);
INSERT INTO retention_activity_cursors(stream,kind,subkind) VALUES(15,'compaction',NULL);
INSERT INTO retention_activity_cursors(stream,kind,subkind) VALUES(16,'Diagnostic',NULL);
INSERT INTO retention_activity_cursors(stream,kind,subkind) VALUES(17,'Ownership',NULL);
INSERT INTO retention_activity_cursors(stream,kind,subkind) VALUES(18,'Ledger',NULL);
INSERT INTO retention_activity_cursors(stream,kind,subkind) VALUES(19,'PullRequest',NULL);
INSERT INTO retention_activity_cursors(stream,kind,subkind) VALUES(20,'ConversationPlan',NULL);
INSERT INTO retention_activity_cursors(stream,kind) VALUES(21,'host_input_receipts');
INSERT INTO retention_activity_cursors(stream,kind) VALUES(22,'host_chat_submissions');
INSERT INTO retention_activity_cursors(stream,kind) VALUES(23,'thread_runs');
INSERT INTO retention_activity_cursors(stream,kind) VALUES(24,'run_contexts');
-- Freeze each source's initial range; later writes are handled by the durable queue.
-- All five MAX lookups use the rowid/INTEGER PRIMARY KEY edge, not a payload scan.
UPDATE retention_activity_cursors SET high_water=CASE kind
WHEN 'host_input_receipts' THEN coalesce((SELECT max(rowid) FROM host_input_receipts),0)
WHEN 'host_chat_submissions' THEN coalesce((SELECT max(rowid) FROM host_chat_submissions),0)
WHEN 'thread_runs' THEN coalesce((SELECT max(rowid) FROM thread_runs),0)
WHEN 'run_contexts' THEN coalesce((SELECT max(rowid) FROM run_contexts),0)
ELSE coalesce((SELECT max(id) FROM events),0) END;

CREATE TABLE retention_activity_sources(run_id TEXT PRIMARY KEY, revision INTEGER NOT NULL DEFAULT 0, decision_revision INTEGER NOT NULL DEFAULT 0);
CREATE TABLE retention_activity_pending(source INTEGER NOT NULL, source_id INTEGER NOT NULL, run_id TEXT NOT NULL, revision INTEGER NOT NULL DEFAULT 1, PRIMARY KEY(source,source_id,run_id));
CREATE INDEX retention_activity_pending_run ON retention_activity_pending(run_id);
CREATE TABLE retention_activity_runs(run_id TEXT PRIMARY KEY, actual_ns INTEGER, legacy_upper_ns INTEGER, captured INTEGER NOT NULL DEFAULT 0, legacy INTEGER NOT NULL DEFAULT 0, applied INTEGER NOT NULL DEFAULT -1, failure TEXT);
CREATE INDEX retention_activity_candidates ON retention_activity_runs(failure,actual_ns,run_id);
CREATE TABLE retention_activity_goals(goal_id TEXT PRIMARY KEY, thread_id TEXT NOT NULL, root_run_id TEXT NOT NULL, related TEXT NOT NULL, complete INTEGER NOT NULL, event_id INTEGER NOT NULL, uncertain INTEGER NOT NULL DEFAULT 0);
CREATE INDEX retention_activity_goals_thread ON retention_activity_goals(thread_id);
CREATE INDEX retention_activity_goals_event ON retention_activity_goals(event_id);
CREATE TABLE retention_activity_bindings(thread_id TEXT PRIMARY KEY, goal_id TEXT, event_id INTEGER NOT NULL, uncertain INTEGER NOT NULL DEFAULT 0);
CREATE TABLE retention_activity_deliveries(message_id TEXT PRIMARY KEY, run_id TEXT NOT NULL, accepted_at_ns INTEGER NOT NULL);
CREATE INDEX retention_activity_deliveries_run ON retention_activity_deliveries(run_id);
CREATE TRIGGER retention_activity_events_insert AFTER INSERT ON events WHEN (NEW.subkind IS NULL OR (NEW.kind='Message' AND NEW.subkind IN ('MessageCompleted','FinalResultPublished','MessagePart')) OR (NEW.kind='Lifecycle' AND NEW.subkind IN ('AgentRunStateChanged','TurnCompleted')) OR (NEW.kind='Orchestrator' AND NEW.subkind='ThreadGoalUpdated') OR (NEW.kind='AgentMessage' AND NEW.subkind='Delivered')) BEGIN
UPDATE retention_activity_control SET revision=revision+1 WHERE id=1;
INSERT INTO retention_activity_sources(run_id,revision) SELECT coalesce(NEW.run_id,''),1 WHERE NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=NEW.run_id) ON CONFLICT(run_id) DO UPDATE SET revision=revision+1;
INSERT INTO retention_activity_pending(source,source_id,run_id) SELECT 0,NEW.id,coalesce(NEW.run_id,'') WHERE NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=NEW.run_id) ON CONFLICT(source,source_id,run_id) DO UPDATE SET revision=revision+1;
END;
CREATE TRIGGER retention_activity_events_delete AFTER DELETE ON events WHEN (OLD.subkind IS NULL OR (OLD.kind='Message' AND OLD.subkind IN ('MessageCompleted','FinalResultPublished','MessagePart')) OR (OLD.kind='Lifecycle' AND OLD.subkind IN ('AgentRunStateChanged','TurnCompleted')) OR (OLD.kind='Orchestrator' AND OLD.subkind='ThreadGoalUpdated') OR (OLD.kind='AgentMessage' AND OLD.subkind='Delivered')) BEGIN
UPDATE retention_activity_control SET revision=revision+1 WHERE id=1;
INSERT INTO retention_activity_sources(run_id,revision) SELECT coalesce(OLD.run_id,''),1 WHERE NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=OLD.run_id) ON CONFLICT(run_id) DO UPDATE SET revision=revision+1;
INSERT INTO retention_activity_pending(source,source_id,run_id) SELECT 0,OLD.id,coalesce(OLD.run_id,'') WHERE NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=OLD.run_id) ON CONFLICT(source,source_id,run_id) DO UPDATE SET revision=revision+1;
END;
CREATE TRIGGER retention_activity_events_update AFTER UPDATE OF kind,subkind,run_id,wall_clock_ns ON events WHEN ((OLD.subkind IS NULL OR (OLD.kind='Message' AND OLD.subkind IN ('MessageCompleted','FinalResultPublished','MessagePart')) OR (OLD.kind='Lifecycle' AND OLD.subkind IN ('AgentRunStateChanged','TurnCompleted')) OR (OLD.kind='Orchestrator' AND OLD.subkind='ThreadGoalUpdated') OR (OLD.kind='AgentMessage' AND OLD.subkind='Delivered')) OR (NEW.subkind IS NULL OR (NEW.kind='Message' AND NEW.subkind IN ('MessageCompleted','FinalResultPublished','MessagePart')) OR (NEW.kind='Lifecycle' AND NEW.subkind IN ('AgentRunStateChanged','TurnCompleted')) OR (NEW.kind='Orchestrator' AND NEW.subkind='ThreadGoalUpdated') OR (NEW.kind='AgentMessage' AND NEW.subkind='Delivered')) ) AND (OLD.kind IS NOT NEW.kind OR OLD.subkind IS NOT NEW.subkind OR OLD.run_id IS NOT NEW.run_id OR OLD.wall_clock_ns IS NOT NEW.wall_clock_ns) BEGIN
UPDATE retention_activity_control SET revision=revision+1 WHERE id=1;
INSERT INTO retention_activity_sources(run_id,revision) SELECT coalesce(OLD.run_id,''),1 WHERE NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=OLD.run_id) ON CONFLICT(run_id) DO UPDATE SET revision=revision+1;
INSERT INTO retention_activity_pending(source,source_id,run_id) SELECT 0,OLD.id,coalesce(OLD.run_id,'') WHERE NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=OLD.run_id) ON CONFLICT(source,source_id,run_id) DO UPDATE SET revision=revision+1;
INSERT INTO retention_activity_sources(run_id,revision) SELECT coalesce(NEW.run_id,''),1 WHERE NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=NEW.run_id) ON CONFLICT(run_id) DO UPDATE SET revision=revision+1;
INSERT INTO retention_activity_pending(source,source_id,run_id) SELECT 0,NEW.id,coalesce(NEW.run_id,'') WHERE NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=NEW.run_id) ON CONFLICT(source,source_id,run_id) DO UPDATE SET revision=revision+1;
END;
-- Header-only activity deliberately ignores payload rewrites. Decoded sources,
-- including legacy NULL subkinds, must remain dirty after an out-of-hook rewrite.
-- Decode must establish attribution even if metadata changed in the same edit.
CREATE TRIGGER retention_activity_events_payload_update AFTER UPDATE OF payload ON events
WHEN (NEW.subkind IS NULL
 OR (NEW.kind='Lifecycle' AND NEW.subkind='AgentRunStateChanged')
 OR (NEW.kind='Orchestrator' AND NEW.subkind='ThreadGoalUpdated')
 OR (NEW.kind='AgentMessage' AND NEW.subkind='Delivered'))
AND OLD.payload IS NOT NEW.payload BEGIN
UPDATE retention_activity_control SET revision=revision+1 WHERE id=1;
INSERT INTO retention_activity_sources(run_id,revision) SELECT coalesce(NEW.run_id,''),1 WHERE NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=NEW.run_id) ON CONFLICT(run_id) DO UPDATE SET revision=revision+1;
INSERT INTO retention_activity_pending(source,source_id,run_id) SELECT 0,NEW.id,coalesce(NEW.run_id,'') WHERE NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=NEW.run_id) ON CONFLICT(source,source_id,run_id) DO UPDATE SET revision=revision+1;
-- The rewritten payload may belong to another family despite an unchanged header.
INSERT INTO retention_activity_sources(run_id,revision) SELECT '',1 WHERE coalesce(NEW.run_id,'')<>'' AND NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=NEW.run_id) ON CONFLICT(run_id) DO UPDATE SET revision=revision+1;
INSERT INTO retention_activity_pending(source,source_id,run_id) SELECT 0,NEW.id,'' WHERE coalesce(NEW.run_id,'')<>'' AND NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=NEW.run_id) ON CONFLICT(source,source_id,run_id) DO UPDATE SET revision=revision+1;
END;
CREATE TRIGGER retention_activity_host_input_receipts_insert AFTER INSERT ON host_input_receipts BEGIN
UPDATE retention_activity_control SET revision=revision+1 WHERE id=1;
INSERT INTO retention_activity_sources(run_id,revision) SELECT coalesce(NEW.run_id,''),1 WHERE NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=NEW.run_id) ON CONFLICT(run_id) DO UPDATE SET revision=revision+1;
INSERT INTO retention_activity_pending(source,source_id,run_id) SELECT 1,NEW.rowid,coalesce(NEW.run_id,'') WHERE NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=NEW.run_id) ON CONFLICT(source,source_id,run_id) DO UPDATE SET revision=revision+1;
END;
CREATE TRIGGER retention_activity_host_input_receipts_delete AFTER DELETE ON host_input_receipts BEGIN
UPDATE retention_activity_control SET revision=revision+1 WHERE id=1;
INSERT INTO retention_activity_sources(run_id,revision) SELECT coalesce(OLD.run_id,''),1 WHERE NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=OLD.run_id) ON CONFLICT(run_id) DO UPDATE SET revision=revision+1;
INSERT INTO retention_activity_pending(source,source_id,run_id) SELECT 1,OLD.rowid,coalesce(OLD.run_id,'') WHERE NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=OLD.run_id) ON CONFLICT(source,source_id,run_id) DO UPDATE SET revision=revision+1;
END;
CREATE TRIGGER retention_activity_host_input_receipts_update AFTER UPDATE OF run_id,accepted_at_ns ON host_input_receipts WHEN OLD.run_id IS NOT NEW.run_id OR OLD.accepted_at_ns IS NOT NEW.accepted_at_ns BEGIN
UPDATE retention_activity_control SET revision=revision+1 WHERE id=1;
INSERT INTO retention_activity_sources(run_id,revision) SELECT coalesce(OLD.run_id,''),1 WHERE NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=OLD.run_id) ON CONFLICT(run_id) DO UPDATE SET revision=revision+1;
INSERT INTO retention_activity_pending(source,source_id,run_id) SELECT 1,OLD.rowid,coalesce(OLD.run_id,'') WHERE NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=OLD.run_id) ON CONFLICT(source,source_id,run_id) DO UPDATE SET revision=revision+1;
INSERT INTO retention_activity_sources(run_id,revision) SELECT coalesce(NEW.run_id,''),1 WHERE NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=NEW.run_id) ON CONFLICT(run_id) DO UPDATE SET revision=revision+1;
INSERT INTO retention_activity_pending(source,source_id,run_id) SELECT 1,NEW.rowid,coalesce(NEW.run_id,'') WHERE NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=NEW.run_id) ON CONFLICT(source,source_id,run_id) DO UPDATE SET revision=revision+1;
END;
CREATE TRIGGER retention_activity_host_chat_submissions_insert AFTER INSERT ON host_chat_submissions BEGIN
UPDATE retention_activity_control SET revision=revision+1 WHERE id=1;
INSERT INTO retention_activity_sources(run_id,revision) SELECT coalesce(NEW.run_id,''),1 WHERE NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=NEW.run_id) ON CONFLICT(run_id) DO UPDATE SET revision=revision+1;
INSERT INTO retention_activity_pending(source,source_id,run_id) SELECT 2,NEW.rowid,coalesce(NEW.run_id,'') WHERE NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=NEW.run_id) ON CONFLICT(source,source_id,run_id) DO UPDATE SET revision=revision+1;
END;
CREATE TRIGGER retention_activity_host_chat_submissions_delete AFTER DELETE ON host_chat_submissions BEGIN
UPDATE retention_activity_control SET revision=revision+1 WHERE id=1;
INSERT INTO retention_activity_sources(run_id,revision) SELECT coalesce(OLD.run_id,''),1 WHERE NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=OLD.run_id) ON CONFLICT(run_id) DO UPDATE SET revision=revision+1;
INSERT INTO retention_activity_pending(source,source_id,run_id) SELECT 2,OLD.rowid,coalesce(OLD.run_id,'') WHERE NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=OLD.run_id) ON CONFLICT(source,source_id,run_id) DO UPDATE SET revision=revision+1;
END;
CREATE TRIGGER retention_activity_host_chat_submissions_update AFTER UPDATE OF run_id,accepted_at_ns ON host_chat_submissions WHEN OLD.run_id IS NOT NEW.run_id OR OLD.accepted_at_ns IS NOT NEW.accepted_at_ns BEGIN
UPDATE retention_activity_control SET revision=revision+1 WHERE id=1;
INSERT INTO retention_activity_sources(run_id,revision) SELECT coalesce(OLD.run_id,''),1 WHERE NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=OLD.run_id) ON CONFLICT(run_id) DO UPDATE SET revision=revision+1;
INSERT INTO retention_activity_pending(source,source_id,run_id) SELECT 2,OLD.rowid,coalesce(OLD.run_id,'') WHERE NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=OLD.run_id) ON CONFLICT(source,source_id,run_id) DO UPDATE SET revision=revision+1;
INSERT INTO retention_activity_sources(run_id,revision) SELECT coalesce(NEW.run_id,''),1 WHERE NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=NEW.run_id) ON CONFLICT(run_id) DO UPDATE SET revision=revision+1;
INSERT INTO retention_activity_pending(source,source_id,run_id) SELECT 2,NEW.rowid,coalesce(NEW.run_id,'') WHERE NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=NEW.run_id) ON CONFLICT(source,source_id,run_id) DO UPDATE SET revision=revision+1;
END;
CREATE TRIGGER retention_activity_thread_runs_insert AFTER INSERT ON thread_runs BEGIN
UPDATE retention_activity_control SET revision=revision+1,binding_revision=binding_revision+1 WHERE id=1;
INSERT INTO retention_activity_sources(run_id,revision) SELECT coalesce(NEW.run_id,''),1 WHERE NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=NEW.run_id) ON CONFLICT(run_id) DO UPDATE SET revision=revision+1;
INSERT INTO retention_activity_pending(source,source_id,run_id) SELECT 3,NEW.rowid,coalesce(NEW.run_id,'') WHERE NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=NEW.run_id) ON CONFLICT(source,source_id,run_id) DO UPDATE SET revision=revision+1;
END;
CREATE TRIGGER retention_activity_thread_runs_delete AFTER DELETE ON thread_runs BEGIN
UPDATE retention_activity_control SET revision=revision+1,binding_revision=binding_revision+1 WHERE id=1;
INSERT INTO retention_activity_sources(run_id,revision) SELECT coalesce(OLD.run_id,''),1 WHERE NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=OLD.run_id) ON CONFLICT(run_id) DO UPDATE SET revision=revision+1;
INSERT INTO retention_activity_pending(source,source_id,run_id) SELECT 3,OLD.rowid,coalesce(OLD.run_id,'') WHERE NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=OLD.run_id) ON CONFLICT(source,source_id,run_id) DO UPDATE SET revision=revision+1;
END;
CREATE TRIGGER retention_activity_thread_runs_update AFTER UPDATE OF run_id,thread_id,root_run_id,depth ON thread_runs WHEN OLD.run_id IS NOT NEW.run_id OR OLD.thread_id IS NOT NEW.thread_id OR OLD.root_run_id IS NOT NEW.root_run_id OR OLD.depth IS NOT NEW.depth BEGIN
UPDATE retention_activity_control SET revision=revision+1,binding_revision=binding_revision+1 WHERE id=1;
INSERT INTO retention_activity_sources(run_id,revision) SELECT coalesce(OLD.run_id,''),1 WHERE NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=OLD.run_id) ON CONFLICT(run_id) DO UPDATE SET revision=revision+1;
INSERT INTO retention_activity_pending(source,source_id,run_id) SELECT 3,OLD.rowid,coalesce(OLD.run_id,'') WHERE NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=OLD.run_id) ON CONFLICT(source,source_id,run_id) DO UPDATE SET revision=revision+1;
INSERT INTO retention_activity_sources(run_id,revision) SELECT coalesce(NEW.run_id,''),1 WHERE NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=NEW.run_id) ON CONFLICT(run_id) DO UPDATE SET revision=revision+1;
INSERT INTO retention_activity_pending(source,source_id,run_id) SELECT 3,NEW.rowid,coalesce(NEW.run_id,'') WHERE NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=NEW.run_id) ON CONFLICT(source,source_id,run_id) DO UPDATE SET revision=revision+1;
END;
CREATE TRIGGER fence_retention_activity_sources_insert BEFORE INSERT ON retention_activity_sources WHEN NEW.run_id IN (SELECT run_id FROM run_tombstones) BEGIN SELECT RAISE(ABORT,'run or thread is tombstoned'); END;
CREATE TRIGGER fence_retention_activity_sources_update BEFORE UPDATE ON retention_activity_sources WHEN NEW.run_id IN (SELECT run_id FROM run_tombstones) BEGIN SELECT RAISE(ABORT,'run or thread is tombstoned'); END;
CREATE TRIGGER fence_retention_activity_pending_insert BEFORE INSERT ON retention_activity_pending WHEN NEW.run_id IN (SELECT run_id FROM run_tombstones) BEGIN SELECT RAISE(ABORT,'run or thread is tombstoned'); END;
CREATE TRIGGER fence_retention_activity_pending_update BEFORE UPDATE ON retention_activity_pending WHEN NEW.run_id IN (SELECT run_id FROM run_tombstones) BEGIN SELECT RAISE(ABORT,'run or thread is tombstoned'); END;
CREATE TRIGGER fence_retention_activity_runs_insert BEFORE INSERT ON retention_activity_runs WHEN NEW.run_id IN (SELECT run_id FROM run_tombstones) BEGIN SELECT RAISE(ABORT,'run or thread is tombstoned'); END;
CREATE TRIGGER fence_retention_activity_runs_update BEFORE UPDATE ON retention_activity_runs WHEN NEW.run_id IN (SELECT run_id FROM run_tombstones) BEGIN SELECT RAISE(ABORT,'run or thread is tombstoned'); END;
CREATE TRIGGER fence_retention_activity_deliveries_insert BEFORE INSERT ON retention_activity_deliveries WHEN NEW.run_id IN (SELECT run_id FROM run_tombstones) BEGIN SELECT RAISE(ABORT,'run or thread is tombstoned'); END;
CREATE TRIGGER fence_retention_activity_deliveries_update BEFORE UPDATE ON retention_activity_deliveries WHEN NEW.run_id IN (SELECT run_id FROM run_tombstones) BEGIN SELECT RAISE(ABORT,'run or thread is tombstoned'); END;
CREATE TRIGGER fence_retention_activity_goals_insert BEFORE INSERT ON retention_activity_goals WHEN NEW.thread_id IN (SELECT thread_id FROM thread_tombstones) OR NEW.root_run_id IN (SELECT run_id FROM run_tombstones) BEGIN SELECT RAISE(ABORT,'run or thread is tombstoned'); END;
CREATE TRIGGER fence_retention_activity_goals_update BEFORE UPDATE ON retention_activity_goals WHEN NEW.thread_id IN (SELECT thread_id FROM thread_tombstones) OR NEW.root_run_id IN (SELECT run_id FROM run_tombstones) BEGIN SELECT RAISE(ABORT,'run or thread is tombstoned'); END;
CREATE TRIGGER fence_retention_activity_bindings_insert BEFORE INSERT ON retention_activity_bindings WHEN NEW.thread_id IN (SELECT thread_id FROM thread_tombstones) BEGIN SELECT RAISE(ABORT,'run or thread is tombstoned'); END;
CREATE TRIGGER fence_retention_activity_bindings_update BEFORE UPDATE ON retention_activity_bindings WHEN NEW.thread_id IN (SELECT thread_id FROM thread_tombstones) BEGIN SELECT RAISE(ABORT,'run or thread is tombstoned'); END;

CREATE TABLE retention_activity_failures(source INTEGER NOT NULL,source_id INTEGER NOT NULL,run_id TEXT NOT NULL,reason TEXT NOT NULL,PRIMARY KEY(source,source_id,run_id));
CREATE INDEX idx_retention_activity_failures_run ON retention_activity_failures(run_id);
CREATE TRIGGER retention_activity_failures_fence_insert BEFORE INSERT ON retention_activity_failures WHEN EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=NEW.run_id) BEGIN SELECT RAISE(ABORT,'tombstoned run'); END;
CREATE TRIGGER retention_activity_failures_fence_update BEFORE UPDATE ON retention_activity_failures WHEN EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=NEW.run_id) BEGIN SELECT RAISE(ABORT,'tombstoned run'); END;

-- Decision epochs are separate from source/applied revisions: materializing a
-- payload can change a different run without changing that run's source clock.
-- Triggers cover every projection write, including raw SQL and crash/replay.
-- Global epochs are reserved for scope-free state (initial cursors/unknown run).
CREATE TABLE retention_activity_thread_revisions(thread_id TEXT PRIMARY KEY, revision INTEGER NOT NULL DEFAULT 0);
CREATE TRIGGER fence_retention_activity_thread_revisions_insert BEFORE INSERT ON retention_activity_thread_revisions WHEN NEW.thread_id IN (SELECT thread_id FROM thread_tombstones) BEGIN SELECT RAISE(ABORT,'thread is tombstoned'); END;
CREATE TRIGGER fence_retention_activity_thread_revisions_update BEFORE UPDATE ON retention_activity_thread_revisions WHEN NEW.thread_id IN (SELECT thread_id FROM thread_tombstones) BEGIN SELECT RAISE(ABORT,'thread is tombstoned'); END;
CREATE TRIGGER retention_decision_runs_insert AFTER INSERT ON retention_activity_runs BEGIN
INSERT INTO retention_activity_sources(run_id,revision,decision_revision) SELECT run_id,0,1 FROM (SELECT NEW.run_id AS run_id) affected WHERE run_id<>'' AND NOT EXISTS(SELECT 1 FROM run_tombstones t WHERE t.run_id=affected.run_id) ON CONFLICT(run_id) DO UPDATE SET decision_revision=retention_activity_sources.decision_revision+1;
UPDATE retention_activity_control SET decision_revision=decision_revision+1 WHERE id=1 AND EXISTS(SELECT 1 FROM (SELECT NEW.run_id AS run_id) WHERE run_id='');
END;
CREATE TRIGGER retention_decision_runs_update AFTER UPDATE ON retention_activity_runs WHEN OLD.run_id IS NOT NEW.run_id OR OLD.actual_ns IS NOT NEW.actual_ns OR OLD.legacy_upper_ns IS NOT NEW.legacy_upper_ns OR OLD.captured IS NOT NEW.captured OR OLD.legacy IS NOT NEW.legacy OR OLD.applied IS NOT NEW.applied OR OLD.failure IS NOT NEW.failure BEGIN
INSERT INTO retention_activity_sources(run_id,revision,decision_revision) SELECT run_id,0,1 FROM (SELECT OLD.run_id AS run_id UNION SELECT NEW.run_id AS run_id) affected WHERE run_id<>'' AND NOT EXISTS(SELECT 1 FROM run_tombstones t WHERE t.run_id=affected.run_id) ON CONFLICT(run_id) DO UPDATE SET decision_revision=retention_activity_sources.decision_revision+1;
UPDATE retention_activity_control SET decision_revision=decision_revision+1 WHERE id=1 AND EXISTS(SELECT 1 FROM (SELECT OLD.run_id AS run_id UNION SELECT NEW.run_id AS run_id) WHERE run_id='');
END;
CREATE TRIGGER retention_decision_runs_delete AFTER DELETE ON retention_activity_runs BEGIN
INSERT INTO retention_activity_sources(run_id,revision,decision_revision) SELECT run_id,0,1 FROM (SELECT OLD.run_id AS run_id) affected WHERE run_id<>'' AND NOT EXISTS(SELECT 1 FROM run_tombstones t WHERE t.run_id=affected.run_id) ON CONFLICT(run_id) DO UPDATE SET decision_revision=retention_activity_sources.decision_revision+1;
UPDATE retention_activity_control SET decision_revision=decision_revision+1 WHERE id=1 AND EXISTS(SELECT 1 FROM (SELECT OLD.run_id AS run_id) WHERE run_id='');
END;
CREATE TRIGGER retention_decision_goals_insert AFTER INSERT ON retention_activity_goals BEGIN
INSERT INTO retention_activity_sources(run_id,revision,decision_revision) SELECT run_id,0,1 FROM (SELECT NEW.root_run_id AS run_id UNION SELECT value AS run_id FROM json_each(CASE WHEN json_valid(NEW.related) THEN NEW.related ELSE '[]' END) WHERE type='text') affected WHERE run_id<>'' AND NOT EXISTS(SELECT 1 FROM run_tombstones t WHERE t.run_id=affected.run_id) ON CONFLICT(run_id) DO UPDATE SET decision_revision=retention_activity_sources.decision_revision+1;
UPDATE retention_activity_control SET decision_revision=decision_revision+1 WHERE id=1 AND EXISTS(SELECT 1 FROM (SELECT NEW.root_run_id AS run_id UNION SELECT value AS run_id FROM json_each(CASE WHEN json_valid(NEW.related) THEN NEW.related ELSE '[]' END) WHERE type='text') WHERE run_id='');
INSERT INTO retention_activity_thread_revisions(thread_id,revision) SELECT thread_id,1 FROM (SELECT NEW.thread_id AS thread_id) affected WHERE thread_id<>'' AND NOT EXISTS(SELECT 1 FROM thread_tombstones t WHERE t.thread_id=affected.thread_id) ON CONFLICT(thread_id) DO UPDATE SET revision=retention_activity_thread_revisions.revision+1;
END;
CREATE TRIGGER retention_decision_goals_update AFTER UPDATE ON retention_activity_goals WHEN OLD.goal_id IS NOT NEW.goal_id OR OLD.thread_id IS NOT NEW.thread_id OR OLD.root_run_id IS NOT NEW.root_run_id OR OLD.related IS NOT NEW.related OR OLD.complete IS NOT NEW.complete OR OLD.event_id IS NOT NEW.event_id OR OLD.uncertain IS NOT NEW.uncertain BEGIN
INSERT INTO retention_activity_sources(run_id,revision,decision_revision) SELECT run_id,0,1 FROM (SELECT OLD.root_run_id AS run_id UNION SELECT value AS run_id FROM json_each(CASE WHEN json_valid(OLD.related) THEN OLD.related ELSE '[]' END) WHERE type='text' UNION SELECT NEW.root_run_id AS run_id UNION SELECT value AS run_id FROM json_each(CASE WHEN json_valid(NEW.related) THEN NEW.related ELSE '[]' END) WHERE type='text') affected WHERE run_id<>'' AND NOT EXISTS(SELECT 1 FROM run_tombstones t WHERE t.run_id=affected.run_id) ON CONFLICT(run_id) DO UPDATE SET decision_revision=retention_activity_sources.decision_revision+1;
UPDATE retention_activity_control SET decision_revision=decision_revision+1 WHERE id=1 AND EXISTS(SELECT 1 FROM (SELECT OLD.root_run_id AS run_id UNION SELECT value AS run_id FROM json_each(CASE WHEN json_valid(OLD.related) THEN OLD.related ELSE '[]' END) WHERE type='text' UNION SELECT NEW.root_run_id AS run_id UNION SELECT value AS run_id FROM json_each(CASE WHEN json_valid(NEW.related) THEN NEW.related ELSE '[]' END) WHERE type='text') WHERE run_id='');
INSERT INTO retention_activity_thread_revisions(thread_id,revision) SELECT thread_id,1 FROM (SELECT OLD.thread_id AS thread_id UNION SELECT NEW.thread_id AS thread_id) affected WHERE thread_id<>'' AND NOT EXISTS(SELECT 1 FROM thread_tombstones t WHERE t.thread_id=affected.thread_id) ON CONFLICT(thread_id) DO UPDATE SET revision=retention_activity_thread_revisions.revision+1;
END;
CREATE TRIGGER retention_decision_goals_delete AFTER DELETE ON retention_activity_goals BEGIN
INSERT INTO retention_activity_sources(run_id,revision,decision_revision) SELECT run_id,0,1 FROM (SELECT OLD.root_run_id AS run_id UNION SELECT value AS run_id FROM json_each(CASE WHEN json_valid(OLD.related) THEN OLD.related ELSE '[]' END) WHERE type='text') affected WHERE run_id<>'' AND NOT EXISTS(SELECT 1 FROM run_tombstones t WHERE t.run_id=affected.run_id) ON CONFLICT(run_id) DO UPDATE SET decision_revision=retention_activity_sources.decision_revision+1;
UPDATE retention_activity_control SET decision_revision=decision_revision+1 WHERE id=1 AND EXISTS(SELECT 1 FROM (SELECT OLD.root_run_id AS run_id UNION SELECT value AS run_id FROM json_each(CASE WHEN json_valid(OLD.related) THEN OLD.related ELSE '[]' END) WHERE type='text') WHERE run_id='');
INSERT INTO retention_activity_thread_revisions(thread_id,revision) SELECT thread_id,1 FROM (SELECT OLD.thread_id AS thread_id) affected WHERE thread_id<>'' AND NOT EXISTS(SELECT 1 FROM thread_tombstones t WHERE t.thread_id=affected.thread_id) ON CONFLICT(thread_id) DO UPDATE SET revision=retention_activity_thread_revisions.revision+1;
END;
CREATE TRIGGER retention_decision_bindings_insert AFTER INSERT ON retention_activity_bindings BEGIN
INSERT INTO retention_activity_sources(run_id,revision,decision_revision) SELECT run_id,0,1 FROM (SELECT root_run_id AS run_id FROM retention_activity_goals WHERE goal_id=NEW.goal_id UNION SELECT j.value AS run_id FROM retention_activity_goals g,json_each(CASE WHEN json_valid(g.related) THEN g.related ELSE '[]' END) j WHERE g.goal_id=NEW.goal_id AND j.type='text') affected WHERE run_id<>'' AND NOT EXISTS(SELECT 1 FROM run_tombstones t WHERE t.run_id=affected.run_id) ON CONFLICT(run_id) DO UPDATE SET decision_revision=retention_activity_sources.decision_revision+1;
UPDATE retention_activity_control SET decision_revision=decision_revision+1 WHERE id=1 AND EXISTS(SELECT 1 FROM (SELECT root_run_id AS run_id FROM retention_activity_goals WHERE goal_id=NEW.goal_id UNION SELECT j.value AS run_id FROM retention_activity_goals g,json_each(CASE WHEN json_valid(g.related) THEN g.related ELSE '[]' END) j WHERE g.goal_id=NEW.goal_id AND j.type='text') WHERE run_id='');
INSERT INTO retention_activity_thread_revisions(thread_id,revision) SELECT thread_id,1 FROM (SELECT NEW.thread_id AS thread_id) affected WHERE thread_id<>'' AND NOT EXISTS(SELECT 1 FROM thread_tombstones t WHERE t.thread_id=affected.thread_id) ON CONFLICT(thread_id) DO UPDATE SET revision=retention_activity_thread_revisions.revision+1;
END;
CREATE TRIGGER retention_decision_bindings_update AFTER UPDATE ON retention_activity_bindings WHEN OLD.thread_id IS NOT NEW.thread_id OR OLD.goal_id IS NOT NEW.goal_id OR OLD.event_id IS NOT NEW.event_id OR OLD.uncertain IS NOT NEW.uncertain BEGIN
INSERT INTO retention_activity_sources(run_id,revision,decision_revision) SELECT run_id,0,1 FROM (SELECT root_run_id AS run_id FROM retention_activity_goals WHERE goal_id=OLD.goal_id UNION SELECT j.value AS run_id FROM retention_activity_goals g,json_each(CASE WHEN json_valid(g.related) THEN g.related ELSE '[]' END) j WHERE g.goal_id=OLD.goal_id AND j.type='text' UNION SELECT root_run_id AS run_id FROM retention_activity_goals WHERE goal_id=NEW.goal_id UNION SELECT j.value AS run_id FROM retention_activity_goals g,json_each(CASE WHEN json_valid(g.related) THEN g.related ELSE '[]' END) j WHERE g.goal_id=NEW.goal_id AND j.type='text') affected WHERE run_id<>'' AND NOT EXISTS(SELECT 1 FROM run_tombstones t WHERE t.run_id=affected.run_id) ON CONFLICT(run_id) DO UPDATE SET decision_revision=retention_activity_sources.decision_revision+1;
UPDATE retention_activity_control SET decision_revision=decision_revision+1 WHERE id=1 AND EXISTS(SELECT 1 FROM (SELECT root_run_id AS run_id FROM retention_activity_goals WHERE goal_id=OLD.goal_id UNION SELECT j.value AS run_id FROM retention_activity_goals g,json_each(CASE WHEN json_valid(g.related) THEN g.related ELSE '[]' END) j WHERE g.goal_id=OLD.goal_id AND j.type='text' UNION SELECT root_run_id AS run_id FROM retention_activity_goals WHERE goal_id=NEW.goal_id UNION SELECT j.value AS run_id FROM retention_activity_goals g,json_each(CASE WHEN json_valid(g.related) THEN g.related ELSE '[]' END) j WHERE g.goal_id=NEW.goal_id AND j.type='text') WHERE run_id='');
INSERT INTO retention_activity_thread_revisions(thread_id,revision) SELECT thread_id,1 FROM (SELECT OLD.thread_id AS thread_id UNION SELECT NEW.thread_id AS thread_id) affected WHERE thread_id<>'' AND NOT EXISTS(SELECT 1 FROM thread_tombstones t WHERE t.thread_id=affected.thread_id) ON CONFLICT(thread_id) DO UPDATE SET revision=retention_activity_thread_revisions.revision+1;
END;
CREATE TRIGGER retention_decision_bindings_delete AFTER DELETE ON retention_activity_bindings BEGIN
INSERT INTO retention_activity_sources(run_id,revision,decision_revision) SELECT run_id,0,1 FROM (SELECT root_run_id AS run_id FROM retention_activity_goals WHERE goal_id=OLD.goal_id UNION SELECT j.value AS run_id FROM retention_activity_goals g,json_each(CASE WHEN json_valid(g.related) THEN g.related ELSE '[]' END) j WHERE g.goal_id=OLD.goal_id AND j.type='text') affected WHERE run_id<>'' AND NOT EXISTS(SELECT 1 FROM run_tombstones t WHERE t.run_id=affected.run_id) ON CONFLICT(run_id) DO UPDATE SET decision_revision=retention_activity_sources.decision_revision+1;
UPDATE retention_activity_control SET decision_revision=decision_revision+1 WHERE id=1 AND EXISTS(SELECT 1 FROM (SELECT root_run_id AS run_id FROM retention_activity_goals WHERE goal_id=OLD.goal_id UNION SELECT j.value AS run_id FROM retention_activity_goals g,json_each(CASE WHEN json_valid(g.related) THEN g.related ELSE '[]' END) j WHERE g.goal_id=OLD.goal_id AND j.type='text') WHERE run_id='');
INSERT INTO retention_activity_thread_revisions(thread_id,revision) SELECT thread_id,1 FROM (SELECT OLD.thread_id AS thread_id) affected WHERE thread_id<>'' AND NOT EXISTS(SELECT 1 FROM thread_tombstones t WHERE t.thread_id=affected.thread_id) ON CONFLICT(thread_id) DO UPDATE SET revision=retention_activity_thread_revisions.revision+1;
END;
CREATE TRIGGER retention_decision_deliveries_insert AFTER INSERT ON retention_activity_deliveries BEGIN
INSERT INTO retention_activity_sources(run_id,revision,decision_revision) SELECT run_id,0,1 FROM (SELECT NEW.run_id AS run_id) affected WHERE run_id<>'' AND NOT EXISTS(SELECT 1 FROM run_tombstones t WHERE t.run_id=affected.run_id) ON CONFLICT(run_id) DO UPDATE SET decision_revision=retention_activity_sources.decision_revision+1;
UPDATE retention_activity_control SET decision_revision=decision_revision+1 WHERE id=1 AND EXISTS(SELECT 1 FROM (SELECT NEW.run_id AS run_id) WHERE run_id='');
END;
CREATE TRIGGER retention_decision_deliveries_update AFTER UPDATE ON retention_activity_deliveries WHEN OLD.message_id IS NOT NEW.message_id OR OLD.run_id IS NOT NEW.run_id OR OLD.accepted_at_ns IS NOT NEW.accepted_at_ns BEGIN
INSERT INTO retention_activity_sources(run_id,revision,decision_revision) SELECT run_id,0,1 FROM (SELECT OLD.run_id AS run_id UNION SELECT NEW.run_id AS run_id) affected WHERE run_id<>'' AND NOT EXISTS(SELECT 1 FROM run_tombstones t WHERE t.run_id=affected.run_id) ON CONFLICT(run_id) DO UPDATE SET decision_revision=retention_activity_sources.decision_revision+1;
UPDATE retention_activity_control SET decision_revision=decision_revision+1 WHERE id=1 AND EXISTS(SELECT 1 FROM (SELECT OLD.run_id AS run_id UNION SELECT NEW.run_id AS run_id) WHERE run_id='');
END;
CREATE TRIGGER retention_decision_deliveries_delete AFTER DELETE ON retention_activity_deliveries BEGIN
INSERT INTO retention_activity_sources(run_id,revision,decision_revision) SELECT run_id,0,1 FROM (SELECT OLD.run_id AS run_id) affected WHERE run_id<>'' AND NOT EXISTS(SELECT 1 FROM run_tombstones t WHERE t.run_id=affected.run_id) ON CONFLICT(run_id) DO UPDATE SET decision_revision=retention_activity_sources.decision_revision+1;
UPDATE retention_activity_control SET decision_revision=decision_revision+1 WHERE id=1 AND EXISTS(SELECT 1 FROM (SELECT OLD.run_id AS run_id) WHERE run_id='');
END;
CREATE TRIGGER retention_decision_failures_insert AFTER INSERT ON retention_activity_failures BEGIN
INSERT INTO retention_activity_sources(run_id,revision,decision_revision) SELECT run_id,0,1 FROM (SELECT NEW.run_id AS run_id) affected WHERE run_id<>'' AND NOT EXISTS(SELECT 1 FROM run_tombstones t WHERE t.run_id=affected.run_id) ON CONFLICT(run_id) DO UPDATE SET decision_revision=retention_activity_sources.decision_revision+1;
UPDATE retention_activity_control SET decision_revision=decision_revision+1 WHERE id=1 AND EXISTS(SELECT 1 FROM (SELECT NEW.run_id AS run_id) WHERE run_id='');
END;
CREATE TRIGGER retention_decision_failures_update AFTER UPDATE ON retention_activity_failures WHEN OLD.source IS NOT NEW.source OR OLD.source_id IS NOT NEW.source_id OR OLD.run_id IS NOT NEW.run_id OR OLD.reason IS NOT NEW.reason BEGIN
INSERT INTO retention_activity_sources(run_id,revision,decision_revision) SELECT run_id,0,1 FROM (SELECT OLD.run_id AS run_id UNION SELECT NEW.run_id AS run_id) affected WHERE run_id<>'' AND NOT EXISTS(SELECT 1 FROM run_tombstones t WHERE t.run_id=affected.run_id) ON CONFLICT(run_id) DO UPDATE SET decision_revision=retention_activity_sources.decision_revision+1;
UPDATE retention_activity_control SET decision_revision=decision_revision+1 WHERE id=1 AND EXISTS(SELECT 1 FROM (SELECT OLD.run_id AS run_id UNION SELECT NEW.run_id AS run_id) WHERE run_id='');
END;
CREATE TRIGGER retention_decision_failures_delete AFTER DELETE ON retention_activity_failures BEGIN
INSERT INTO retention_activity_sources(run_id,revision,decision_revision) SELECT run_id,0,1 FROM (SELECT OLD.run_id AS run_id) affected WHERE run_id<>'' AND NOT EXISTS(SELECT 1 FROM run_tombstones t WHERE t.run_id=affected.run_id) ON CONFLICT(run_id) DO UPDATE SET decision_revision=retention_activity_sources.decision_revision+1;
UPDATE retention_activity_control SET decision_revision=decision_revision+1 WHERE id=1 AND EXISTS(SELECT 1 FROM (SELECT OLD.run_id AS run_id) WHERE run_id='');
END;
CREATE TRIGGER retention_decision_pending_insert AFTER INSERT ON retention_activity_pending BEGIN
INSERT INTO retention_activity_sources(run_id,revision,decision_revision) SELECT run_id,0,1 FROM (SELECT NEW.run_id AS run_id) affected WHERE run_id<>'' AND NOT EXISTS(SELECT 1 FROM run_tombstones t WHERE t.run_id=affected.run_id) ON CONFLICT(run_id) DO UPDATE SET decision_revision=retention_activity_sources.decision_revision+1;
UPDATE retention_activity_control SET decision_revision=decision_revision+1 WHERE id=1 AND EXISTS(SELECT 1 FROM (SELECT NEW.run_id AS run_id) WHERE run_id='');
END;
CREATE TRIGGER retention_decision_pending_update AFTER UPDATE ON retention_activity_pending WHEN OLD.source IS NOT NEW.source OR OLD.source_id IS NOT NEW.source_id OR OLD.run_id IS NOT NEW.run_id OR OLD.revision IS NOT NEW.revision BEGIN
INSERT INTO retention_activity_sources(run_id,revision,decision_revision) SELECT run_id,0,1 FROM (SELECT OLD.run_id AS run_id UNION SELECT NEW.run_id AS run_id) affected WHERE run_id<>'' AND NOT EXISTS(SELECT 1 FROM run_tombstones t WHERE t.run_id=affected.run_id) ON CONFLICT(run_id) DO UPDATE SET decision_revision=retention_activity_sources.decision_revision+1;
UPDATE retention_activity_control SET decision_revision=decision_revision+1 WHERE id=1 AND EXISTS(SELECT 1 FROM (SELECT OLD.run_id AS run_id UNION SELECT NEW.run_id AS run_id) WHERE run_id='');
END;
CREATE TRIGGER retention_decision_pending_delete AFTER DELETE ON retention_activity_pending BEGIN
INSERT INTO retention_activity_sources(run_id,revision,decision_revision) SELECT run_id,0,1 FROM (SELECT OLD.run_id AS run_id) affected WHERE run_id<>'' AND NOT EXISTS(SELECT 1 FROM run_tombstones t WHERE t.run_id=affected.run_id) ON CONFLICT(run_id) DO UPDATE SET decision_revision=retention_activity_sources.decision_revision+1;
UPDATE retention_activity_control SET decision_revision=decision_revision+1 WHERE id=1 AND EXISTS(SELECT 1 FROM (SELECT OLD.run_id AS run_id) WHERE run_id='');
END;
CREATE TRIGGER retention_decision_cursors_insert AFTER INSERT ON retention_activity_cursors BEGIN
UPDATE retention_activity_control SET decision_revision=decision_revision+1 WHERE id=1;
END;
CREATE TRIGGER retention_decision_cursors_update AFTER UPDATE ON retention_activity_cursors WHEN OLD.stream IS NOT NEW.stream OR OLD.kind IS NOT NEW.kind OR OLD.subkind IS NOT NEW.subkind OR OLD.after_id IS NOT NEW.after_id OR OLD.high_water IS NOT NEW.high_water OR OLD.complete IS NOT NEW.complete BEGIN
UPDATE retention_activity_control SET decision_revision=decision_revision+1 WHERE id=1;
END;
CREATE TRIGGER retention_decision_cursors_delete AFTER DELETE ON retention_activity_cursors BEGIN
UPDATE retention_activity_control SET decision_revision=decision_revision+1 WHERE id=1;
END;
CREATE TRIGGER retention_decision_sources_insert AFTER INSERT ON retention_activity_sources WHEN NEW.decision_revision<>0 BEGIN
INSERT INTO retention_activity_thread_revisions(thread_id,revision) SELECT thread_id,1 FROM (SELECT thread_id FROM thread_runs WHERE run_id=NEW.run_id) affected WHERE thread_id<>'' AND NOT EXISTS(SELECT 1 FROM thread_tombstones t WHERE t.thread_id=affected.thread_id) ON CONFLICT(thread_id) DO UPDATE SET revision=retention_activity_thread_revisions.revision+1;
END;
CREATE TRIGGER retention_decision_sources_update AFTER UPDATE OF decision_revision,run_id ON retention_activity_sources WHEN OLD.decision_revision IS NOT NEW.decision_revision OR OLD.run_id IS NOT NEW.run_id BEGIN
INSERT INTO retention_activity_thread_revisions(thread_id,revision) SELECT thread_id,1 FROM (SELECT thread_id FROM thread_runs WHERE run_id=OLD.run_id UNION SELECT thread_id FROM thread_runs WHERE run_id=NEW.run_id) affected WHERE thread_id<>'' AND NOT EXISTS(SELECT 1 FROM thread_tombstones t WHERE t.thread_id=affected.thread_id) ON CONFLICT(thread_id) DO UPDATE SET revision=retention_activity_thread_revisions.revision+1;
UPDATE retention_activity_control SET decision_revision=decision_revision+1 WHERE id=1 AND (NEW.decision_revision<OLD.decision_revision OR OLD.run_id IS NOT NEW.run_id);
END;
CREATE TRIGGER retention_decision_sources_delete AFTER DELETE ON retention_activity_sources WHEN NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=OLD.run_id) BEGIN UPDATE retention_activity_control SET decision_revision=decision_revision+1 WHERE id=1; END;
CREATE TRIGGER retention_decision_threads_delete AFTER DELETE ON retention_activity_thread_revisions WHEN NOT EXISTS(SELECT 1 FROM thread_tombstones WHERE thread_id=OLD.thread_id) BEGIN UPDATE retention_activity_control SET decision_revision=decision_revision+1 WHERE id=1; END;
CREATE TRIGGER retention_decision_threads_update AFTER UPDATE ON retention_activity_thread_revisions WHEN NEW.revision<OLD.revision OR NEW.thread_id IS NOT OLD.thread_id BEGIN UPDATE retention_activity_control SET decision_revision=decision_revision+1 WHERE id=1; END;
-- A run moving between threads changes both threads' decisions in this transaction.
CREATE TRIGGER retention_decision_thread_runs_insert AFTER INSERT ON thread_runs BEGIN
INSERT INTO retention_activity_thread_revisions(thread_id,revision) SELECT thread_id,1 FROM (SELECT NEW.thread_id AS thread_id) affected WHERE thread_id<>'' AND NOT EXISTS(SELECT 1 FROM thread_tombstones t WHERE t.thread_id=affected.thread_id) ON CONFLICT(thread_id) DO UPDATE SET revision=retention_activity_thread_revisions.revision+1;
END;
CREATE TRIGGER retention_decision_thread_runs_update AFTER UPDATE OF run_id,thread_id,root_run_id,depth ON thread_runs WHEN OLD.run_id IS NOT NEW.run_id OR OLD.thread_id IS NOT NEW.thread_id OR OLD.root_run_id IS NOT NEW.root_run_id OR OLD.depth IS NOT NEW.depth BEGIN
INSERT INTO retention_activity_thread_revisions(thread_id,revision) SELECT thread_id,1 FROM (SELECT OLD.thread_id AS thread_id UNION SELECT NEW.thread_id AS thread_id) affected WHERE thread_id<>'' AND NOT EXISTS(SELECT 1 FROM thread_tombstones t WHERE t.thread_id=affected.thread_id) ON CONFLICT(thread_id) DO UPDATE SET revision=retention_activity_thread_revisions.revision+1;
END;
CREATE TRIGGER retention_decision_thread_runs_delete AFTER DELETE ON thread_runs BEGIN
INSERT INTO retention_activity_thread_revisions(thread_id,revision) SELECT thread_id,1 FROM (SELECT OLD.thread_id AS thread_id) affected WHERE thread_id<>'' AND NOT EXISTS(SELECT 1 FROM thread_tombstones t WHERE t.thread_id=affected.thread_id) ON CONFLICT(thread_id) DO UPDATE SET revision=retention_activity_thread_revisions.revision+1;
END;
