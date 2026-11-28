DROP TRIGGER IF EXISTS history_event_insert;
DROP TRIGGER IF EXISTS history_event_update;
DROP TRIGGER IF EXISTS history_event_delete;
CREATE TRIGGER history_event_insert AFTER INSERT ON events
WHEN NEW.run_id IS NOT NULL AND (NEW.kind = 'Message' AND NEW.subkind IN ('MessageCompleted','ReasoningCompleted','FinalResultPublished','MessagePart') OR NEW.kind = 'Lifecycle' AND NEW.subkind IN ('TaskPromptPublished','AgentRunStateChanged') OR NEW.kind = 'Tool' AND NEW.subkind IN ('ToolStarted','ToolCompleted') OR NEW.kind = 'Provider' AND NEW.subkind IN ('RequestFailed','CooldownStarted','FallbackTriggered'))
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
WHEN (OLD.kind = 'Message' AND OLD.subkind IN ('MessageCompleted','ReasoningCompleted','FinalResultPublished','MessagePart') OR OLD.kind = 'Lifecycle' AND OLD.subkind IN ('TaskPromptPublished','AgentRunStateChanged') OR OLD.kind = 'Tool' AND OLD.subkind IN ('ToolStarted','ToolCompleted') OR OLD.kind = 'Provider' AND OLD.subkind IN ('RequestFailed','CooldownStarted','FallbackTriggered')) OR (NEW.kind = 'Message' AND NEW.subkind IN ('MessageCompleted','ReasoningCompleted','FinalResultPublished','MessagePart') OR NEW.kind = 'Lifecycle' AND NEW.subkind IN ('TaskPromptPublished','AgentRunStateChanged') OR NEW.kind = 'Tool' AND NEW.subkind IN ('ToolStarted','ToolCompleted') OR NEW.kind = 'Provider' AND NEW.subkind IN ('RequestFailed','CooldownStarted','FallbackTriggered'))
BEGIN
 INSERT INTO history_run_state(run_id, source, generation) SELECT OLD.run_id, 1, 1 WHERE OLD.run_id IS NOT NULL AND (OLD.kind = 'Message' AND OLD.subkind IN ('MessageCompleted','ReasoningCompleted','FinalResultPublished','MessagePart') OR OLD.kind = 'Lifecycle' AND OLD.subkind IN ('TaskPromptPublished','AgentRunStateChanged') OR OLD.kind = 'Tool' AND OLD.subkind IN ('ToolStarted','ToolCompleted') OR OLD.kind = 'Provider' AND OLD.subkind IN ('RequestFailed','CooldownStarted','FallbackTriggered')) AND NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id = OLD.run_id) ON CONFLICT(run_id) DO UPDATE SET source = CASE WHEN source < 9223372036854775807 THEN source + 1 END, generation = CASE WHEN generation < 9223372036854775807 THEN generation + 1 END, phase = 0;
 INSERT INTO history_run_state(run_id, source, generation) SELECT NEW.run_id, 1, 1 WHERE NEW.run_id IS NOT NULL AND (NEW.kind = 'Message' AND NEW.subkind IN ('MessageCompleted','ReasoningCompleted','FinalResultPublished','MessagePart') OR NEW.kind = 'Lifecycle' AND NEW.subkind IN ('TaskPromptPublished','AgentRunStateChanged') OR NEW.kind = 'Tool' AND NEW.subkind IN ('ToolStarted','ToolCompleted') OR NEW.kind = 'Provider' AND NEW.subkind IN ('RequestFailed','CooldownStarted','FallbackTriggered')) AND NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id = NEW.run_id) ON CONFLICT(run_id) DO UPDATE SET source = CASE WHEN source < 9223372036854775807 THEN source + 1 END, generation = CASE WHEN generation < 9223372036854775807 THEN generation + 1 END, phase = 0;
END;
CREATE TRIGGER history_event_delete AFTER DELETE ON events
WHEN (OLD.kind = 'Message' AND OLD.subkind IN ('MessageCompleted','ReasoningCompleted','FinalResultPublished','MessagePart') OR OLD.kind = 'Lifecycle' AND OLD.subkind IN ('TaskPromptPublished','AgentRunStateChanged') OR OLD.kind = 'Tool' AND OLD.subkind IN ('ToolStarted','ToolCompleted') OR OLD.kind = 'Provider' AND OLD.subkind IN ('RequestFailed','CooldownStarted','FallbackTriggered'))
BEGIN
 INSERT INTO history_run_state(run_id, source, generation) SELECT OLD.run_id, 1, 1 WHERE OLD.run_id IS NOT NULL AND (OLD.kind = 'Message' AND OLD.subkind IN ('MessageCompleted','ReasoningCompleted','FinalResultPublished','MessagePart') OR OLD.kind = 'Lifecycle' AND OLD.subkind IN ('TaskPromptPublished','AgentRunStateChanged') OR OLD.kind = 'Tool' AND OLD.subkind IN ('ToolStarted','ToolCompleted') OR OLD.kind = 'Provider' AND OLD.subkind IN ('RequestFailed','CooldownStarted','FallbackTriggered')) AND NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id = OLD.run_id) ON CONFLICT(run_id) DO UPDATE SET source = CASE WHEN source < 9223372036854775807 THEN source + 1 END, generation = CASE WHEN generation < 9223372036854775807 THEN generation + 1 END, phase = 0;
END;
UPDATE history_run_state SET phase=0,
 source=CASE WHEN source < 9223372036854775807 THEN source + 1 END,
 generation=CASE WHEN generation < 9223372036854775807 THEN generation + 1 END;
UPDATE history_control SET reducer_version=4,
 generation=CASE WHEN generation < 9223372036854775807 THEN generation + 1 END WHERE id=1;
