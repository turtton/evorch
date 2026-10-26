-- Indexed task lookup keeps maintenance independent of unrelated event history.
-- Unknown/malformed records are deliberately not retention candidates.
CREATE INDEX IF NOT EXISTS idx_events_durable_retention ON events(
    json_extract(payload, '$.payload.payload.task_id'), id
) WHERE kind = 'Orchestrator' AND json_valid(payload)
    AND json_extract(payload, '$.payload.kind') IN ('TaskProgressed', 'TaskCheckpoint');

CREATE INDEX IF NOT EXISTS idx_events_task_lifecycle ON events(json_extract(payload, '$.payload.payload.task_id'), id)
WHERE kind = 'Lifecycle' AND json_valid(payload);
