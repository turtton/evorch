CREATE TABLE tasks_v8 (
    id TEXT PRIMARY KEY,
    session_id TEXT REFERENCES sessions(id),
    status TEXT NOT NULL CHECK(status IN ('pending','queued','running','blocked','retrying','completed','cancelled','failed')),
    created_at_ns INTEGER NOT NULL,
    updated_at_ns INTEGER NOT NULL,
    parent_run_id TEXT,
    input_json TEXT,
    progress_json TEXT,
    last_artifact_json TEXT,
    failure_reason TEXT,
    resume_cursor_json TEXT,
    attempts INTEGER NOT NULL DEFAULT 0,
    heartbeat_at_ns INTEGER
);
INSERT INTO tasks_v8(id, session_id, status, created_at_ns, updated_at_ns)
SELECT id, session_id, status, created_at_ns, updated_at_ns FROM tasks;
CREATE TEMP TABLE task_links_v8 AS SELECT blocker_id, blocked_id FROM task_links;
DROP TABLE task_links;
DROP TABLE tasks;
ALTER TABLE tasks_v8 RENAME TO tasks;
CREATE INDEX idx_tasks_session_id ON tasks(session_id);
CREATE TABLE task_links (
    blocker_id TEXT NOT NULL REFERENCES tasks(id),
    blocked_id TEXT NOT NULL REFERENCES tasks(id),
    PRIMARY KEY(blocker_id, blocked_id),
    CHECK(blocker_id <> blocked_id)
);
INSERT INTO task_links SELECT blocker_id, blocked_id FROM task_links_v8;
DROP TABLE task_links_v8;
CREATE INDEX idx_task_links_blocked ON task_links(blocked_id);
