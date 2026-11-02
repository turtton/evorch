-- apply_migrations supplies the transaction and user_version guard.
-- A lesson's favorite flag (the user's approval mark) is independent of its status.
-- The projection carries the flag and refuses the one combination the product forbids
-- (a rejected favorite). Each flag change is also kept as a ledger row of kind
-- 'favorite' / 'unfavorite'; it is history only and never moves the projection's
-- ledger_seq, which orders what task prompts receive.
-- SQLite cannot widen a CHECK in place, so rebuild the ledger and the projection that
-- references it (like v10, so foreign_keys can remain enabled). Only the pre-v39
-- columns are copied, which also accepts a database that already has the new ones.
-- Every sequence number and projection rowid is preserved, so no lesson changes its
-- position in ordered listings and the external-content index stays aligned.
CREATE TEMP TABLE memory_ledger_v39 AS
    SELECT seq, entry_id, project, task_id, content, evidence, status, kind, scope
    FROM memory_ledger;
CREATE TEMP TABLE memory_entries_v39 AS
    SELECT rowid AS entry_rowid, id, project, task_id, content, evidence, status,
           ledger_seq, scope
    FROM memory_entries;
DROP TABLE memory_entries;
DROP TABLE memory_ledger;
CREATE TABLE memory_ledger (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    entry_id TEXT NOT NULL,
    project TEXT NOT NULL,
    task_id TEXT NOT NULL,
    content TEXT NOT NULL,
    evidence TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('candidate','validated','promoted','rejected')),
    kind TEXT NOT NULL DEFAULT 'lesson'
        CHECK(kind IN ('lesson', 'finding', 'eval_trace', 'favorite', 'unfavorite')),
    scope TEXT NOT NULL DEFAULT 'project' CHECK(scope IN ('project', 'harness', 'user'))
);
INSERT INTO memory_ledger(seq, entry_id, project, task_id, content, evidence, status, kind, scope)
    SELECT seq, entry_id, project, task_id, content, evidence, status, kind, scope
    FROM memory_ledger_v39 ORDER BY seq;
CREATE INDEX idx_memory_ledger_entry ON memory_ledger(entry_id, seq);
CREATE UNIQUE INDEX idx_eval_trace_identity ON memory_ledger(entry_id) WHERE kind='eval_trace';
CREATE INDEX idx_eval_trace_project ON memory_ledger(project, seq) WHERE kind='eval_trace';
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
    ledger_seq INTEGER NOT NULL REFERENCES memory_ledger(seq),
    scope TEXT NOT NULL DEFAULT 'project' CHECK(scope IN ('project', 'harness', 'user')),
    favorite INTEGER NOT NULL DEFAULT 0 CHECK(favorite IN (0, 1)),
    CHECK(NOT (favorite = 1 AND status = 'rejected'))
);
INSERT INTO memory_entries(rowid, id, project, task_id, content, evidence, status, ledger_seq, scope)
    SELECT entry_rowid, id, project, task_id, content, evidence, status, ledger_seq, scope
    FROM memory_entries_v39 ORDER BY entry_rowid;
CREATE INDEX idx_memory_entries_project_status ON memory_entries(project, status);
CREATE INDEX idx_memory_entries_scope_status ON memory_entries(scope, status);
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
-- The trigger-maintained index was filled before the rebuild; refill it from the rows.
INSERT INTO memory_fts(memory_fts) VALUES('rebuild');
DROP TABLE memory_entries_v39;
DROP TABLE memory_ledger_v39;
