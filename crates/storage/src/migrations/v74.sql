-- Events-only transcript projection. No event index or bulk event rewrite.
CREATE TABLE history_control (
 id INTEGER PRIMARY KEY CHECK(id = 1), format_version INTEGER NOT NULL, reducer_version INTEGER NOT NULL,
 migration_max INTEGER NOT NULL, generation INTEGER, discovery_after INTEGER NOT NULL DEFAULT 0
);
INSERT INTO history_control(id,format_version,reducer_version,migration_max,generation)
 SELECT 1,1,2,COALESCE(MAX(id),0),0 FROM events;
CREATE TABLE history_run_state (
 run_id TEXT PRIMARY KEY, phase INTEGER NOT NULL DEFAULT 0,
 source INTEGER DEFAULT 0, applied INTEGER DEFAULT 0, generation INTEGER DEFAULT 0,
 checkpoint INTEGER, last_indexed_id INTEGER,
 output_count INTEGER NOT NULL DEFAULT 0, merge_max INTEGER,
 first_prompt_id INTEGER, first_prompt_merge_max INTEGER,
 reducer TEXT NOT NULL DEFAULT '{"next":0,"initial_receipt":false,"first_prompt":null,"last_assistant":null}',
 dirty_seen_generation INTEGER DEFAULT -1, retry_at_ns INTEGER NOT NULL DEFAULT 0, failures INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_history_discovery ON history_run_state(phase,retry_at_ns,run_id);
CREATE TABLE history_event_cuts (
 run_id TEXT NOT NULL, event_id INTEGER NOT NULL, before_count INTEGER NOT NULL, after_count INTEGER NOT NULL,
 merge_max INTEGER NOT NULL, PRIMARY KEY(run_id,event_id)
);
CREATE INDEX idx_history_merge ON history_event_cuts(run_id,merge_max,event_id);
CREATE TABLE history_messages (
 run_id TEXT NOT NULL, output_event_id INTEGER NOT NULL, ordinal INTEGER NOT NULL, role TEXT NOT NULL,
 encoded_len INTEGER NOT NULL, PRIMARY KEY(run_id,output_event_id), UNIQUE(run_id,ordinal)
);
CREATE TABLE history_message_sources (
 run_id TEXT NOT NULL, output_event_id INTEGER NOT NULL, part_index INTEGER NOT NULL, event_id INTEGER NOT NULL,
 PRIMARY KEY(run_id,output_event_id,part_index)
);
CREATE TABLE history_pending_parts (
 run_id TEXT NOT NULL, response_id TEXT NOT NULL, part_index INTEGER NOT NULL, total INTEGER NOT NULL, event_id INTEGER NOT NULL,
 PRIMARY KEY(run_id,response_id,part_index)
);

CREATE TRIGGER history_event_insert AFTER INSERT ON events
WHEN NEW.run_id IS NOT NULL AND (NEW.kind = 'Message' AND NEW.subkind IN ('MessageCompleted','FinalResultPublished','MessagePart') OR NEW.kind = 'Lifecycle' AND NEW.subkind = 'TaskPromptPublished' OR NEW.kind = 'Tool' AND NEW.subkind = 'ToolCompleted')
BEGIN
 INSERT INTO history_run_state(run_id,phase,source)
 SELECT NEW.run_id, CASE WHEN (SELECT MIN(id) FROM events WHERE run_id = NEW.run_id) > (SELECT migration_max FROM history_control WHERE id=1) THEN 2 ELSE 0 END, 1
 WHERE NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=NEW.run_id)
 ON CONFLICT(run_id) DO UPDATE SET
 source=CASE WHEN source < 9223372036854775807 THEN source + 1 END,
 -- A scan can pass non-committed rows beyond the last reduced cut. Reusing
 -- one of those IDs must replay that tail, including when no cut exists yet.
 -- This changes only the existing committed INSERT write; non-committed
 -- appends still do not update run state.
 checkpoint=CASE WHEN NEW.id <= checkpoint THEN last_indexed_id ELSE checkpoint END,
 generation=CASE WHEN NEW.id <= last_indexed_id THEN CASE WHEN generation < 9223372036854775807 THEN generation + 1 END ELSE generation END,
 phase=CASE WHEN NEW.id <= last_indexed_id THEN 0 ELSE phase END;
END;
CREATE TRIGGER history_event_update AFTER UPDATE ON events
WHEN (OLD.kind = 'Message' AND OLD.subkind IN ('MessageCompleted','FinalResultPublished','MessagePart') OR OLD.kind = 'Lifecycle' AND OLD.subkind = 'TaskPromptPublished' OR OLD.kind = 'Tool' AND OLD.subkind = 'ToolCompleted') OR (NEW.kind = 'Message' AND NEW.subkind IN ('MessageCompleted','FinalResultPublished','MessagePart') OR NEW.kind = 'Lifecycle' AND NEW.subkind = 'TaskPromptPublished' OR NEW.kind = 'Tool' AND NEW.subkind = 'ToolCompleted')
BEGIN
 INSERT INTO history_run_state(run_id, source, generation) SELECT OLD.run_id, 1, 1 WHERE OLD.run_id IS NOT NULL AND (OLD.kind = 'Message' AND OLD.subkind IN ('MessageCompleted','FinalResultPublished','MessagePart') OR OLD.kind = 'Lifecycle' AND OLD.subkind = 'TaskPromptPublished' OR OLD.kind = 'Tool' AND OLD.subkind = 'ToolCompleted') AND NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id = OLD.run_id) ON CONFLICT(run_id) DO UPDATE SET source = CASE WHEN source < 9223372036854775807 THEN source + 1 END, generation = CASE WHEN generation < 9223372036854775807 THEN generation + 1 END, phase = 0;
 INSERT INTO history_run_state(run_id, source, generation) SELECT NEW.run_id, 1, 1 WHERE NEW.run_id IS NOT NULL AND (NEW.kind = 'Message' AND NEW.subkind IN ('MessageCompleted','FinalResultPublished','MessagePart') OR NEW.kind = 'Lifecycle' AND NEW.subkind = 'TaskPromptPublished' OR NEW.kind = 'Tool' AND NEW.subkind = 'ToolCompleted') AND NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id = NEW.run_id) ON CONFLICT(run_id) DO UPDATE SET source = CASE WHEN source < 9223372036854775807 THEN source + 1 END, generation = CASE WHEN generation < 9223372036854775807 THEN generation + 1 END, phase = 0;
END;
CREATE TRIGGER history_event_delete AFTER DELETE ON events
WHEN (OLD.kind = 'Message' AND OLD.subkind IN ('MessageCompleted','FinalResultPublished','MessagePart') OR OLD.kind = 'Lifecycle' AND OLD.subkind = 'TaskPromptPublished' OR OLD.kind = 'Tool' AND OLD.subkind = 'ToolCompleted')
BEGIN
 INSERT INTO history_run_state(run_id, source, generation) SELECT OLD.run_id, 1, 1 WHERE OLD.run_id IS NOT NULL AND (OLD.kind = 'Message' AND OLD.subkind IN ('MessageCompleted','FinalResultPublished','MessagePart') OR OLD.kind = 'Lifecycle' AND OLD.subkind = 'TaskPromptPublished' OR OLD.kind = 'Tool' AND OLD.subkind = 'ToolCompleted') AND NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id = OLD.run_id) ON CONFLICT(run_id) DO UPDATE SET source = CASE WHEN source < 9223372036854775807 THEN source + 1 END, generation = CASE WHEN generation < 9223372036854775807 THEN generation + 1 END, phase = 0;
END;
CREATE TRIGGER history_unknown_insert AFTER INSERT ON events WHEN NEW.subkind IS NULL
BEGIN UPDATE history_control SET generation=CASE WHEN generation < 9223372036854775807 THEN generation + 1 END WHERE id=1; END;
CREATE TRIGGER history_unknown_update AFTER UPDATE ON events WHEN OLD.subkind IS NULL OR NEW.subkind IS NULL
BEGIN UPDATE history_control SET generation=CASE WHEN generation < 9223372036854775807 THEN generation + 1 END WHERE id=1; END;
CREATE TRIGGER history_unknown_delete AFTER DELETE ON events WHEN OLD.subkind IS NULL
BEGIN UPDATE history_control SET generation=CASE WHEN generation < 9223372036854775807 THEN generation + 1 END WHERE id=1; END;
