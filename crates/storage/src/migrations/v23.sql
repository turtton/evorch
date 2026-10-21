-- Reopen dismissed candidates in place, with durable audit metadata.
ALTER TABLE improvement_candidates ADD COLUMN reopened_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE improvement_candidates ADD COLUMN reopened_at_ns INTEGER;

-- Resume state is carried by the durable run context, not a task-local cursor.
ALTER TABLE tasks DROP COLUMN resume_cursor_json;
