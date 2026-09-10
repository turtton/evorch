ALTER TABLE memory_ledger ADD COLUMN kind TEXT NOT NULL DEFAULT 'lesson'
    CHECK(kind IN ('lesson', 'finding', 'eval_trace'));
CREATE UNIQUE INDEX idx_eval_trace_identity ON memory_ledger(entry_id) WHERE kind='eval_trace';
CREATE INDEX idx_eval_trace_project ON memory_ledger(project, seq) WHERE kind='eval_trace';
