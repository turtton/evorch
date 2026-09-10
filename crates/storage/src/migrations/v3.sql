CREATE TABLE tasks_v3 (
    id TEXT PRIMARY KEY,
    session_id TEXT REFERENCES sessions(id),
    status TEXT NOT NULL CHECK(status IN ('pending','blocked','running','completed','failed')),
    created_at_ns INTEGER NOT NULL,
    updated_at_ns INTEGER NOT NULL
);
INSERT INTO tasks_v3 SELECT * FROM tasks;
DROP TABLE tasks;
ALTER TABLE tasks_v3 RENAME TO tasks;
CREATE INDEX idx_tasks_session_id ON tasks(session_id);
CREATE TABLE task_links (
    blocker_id TEXT NOT NULL REFERENCES tasks(id),
    blocked_id TEXT NOT NULL REFERENCES tasks(id),
    PRIMARY KEY(blocker_id, blocked_id),
    CHECK(blocker_id <> blocked_id)
);
CREATE INDEX idx_task_links_blocked ON task_links(blocked_id);
CREATE TABLE task_queue_ledger (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    operation TEXT NOT NULL,
    payload TEXT NOT NULL
);
CREATE TRIGGER task_queue_no_update BEFORE UPDATE ON task_queue_ledger
BEGIN SELECT RAISE(ABORT, 'task queue ledger is append-only'); END;
CREATE TRIGGER task_queue_no_delete BEFORE DELETE ON task_queue_ledger
BEGIN SELECT RAISE(ABORT, 'task queue ledger is append-only'); END;
CREATE TABLE memory_ledger (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    entry_id TEXT NOT NULL,
    project TEXT NOT NULL,
    task_id TEXT NOT NULL,
    content TEXT NOT NULL,
    evidence TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('candidate','validated','promoted','rejected'))
);
CREATE INDEX idx_memory_ledger_entry ON memory_ledger(entry_id, seq);
CREATE TRIGGER memory_ledger_no_update BEFORE UPDATE ON memory_ledger
BEGIN SELECT RAISE(ABORT, 'memory ledger is append-only'); END;
CREATE TRIGGER memory_ledger_no_delete BEFORE DELETE ON memory_ledger
BEGIN SELECT RAISE(ABORT, 'memory ledger is append-only'); END;
CREATE TABLE memory_entries (
    id TEXT PRIMARY KEY,
    project TEXT NOT NULL,
    task_id TEXT NOT NULL,
    content TEXT NOT NULL,
    evidence TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('candidate','validated','promoted','rejected')),
    ledger_seq INTEGER NOT NULL REFERENCES memory_ledger(seq)
);
CREATE INDEX idx_memory_entries_project_status ON memory_entries(project, status);
CREATE VIRTUAL TABLE memory_fts USING fts5(content, content=memory_entries, content_rowid=rowid);
CREATE TRIGGER memory_entries_insert AFTER INSERT ON memory_entries BEGIN
    INSERT INTO memory_fts(rowid, content) VALUES(new.rowid, new.content);
END;
CREATE TRIGGER memory_entries_update AFTER UPDATE ON memory_entries BEGIN
    INSERT INTO memory_fts(memory_fts, rowid, content) VALUES('delete', old.rowid, old.content);
    INSERT INTO memory_fts(rowid, content) VALUES(new.rowid, new.content);
END;
CREATE TRIGGER memory_entries_delete AFTER DELETE ON memory_entries BEGIN
    INSERT INTO memory_fts(memory_fts, rowid, content) VALUES('delete', old.rowid, old.content);
END;
