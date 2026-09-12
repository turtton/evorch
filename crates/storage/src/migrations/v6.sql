CREATE TABLE run_ledger (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id TEXT NOT NULL,
    body TEXT NOT NULL,
    created_at_ns INTEGER NOT NULL
);
CREATE INDEX idx_run_ledger_run ON run_ledger(run_id, seq);
CREATE TRIGGER run_ledger_no_update BEFORE UPDATE ON run_ledger
BEGIN SELECT RAISE(ABORT, 'run ledger is append-only'); END;
CREATE TRIGGER run_ledger_no_delete BEFORE DELETE ON run_ledger
BEGIN SELECT RAISE(ABORT, 'run ledger is append-only'); END;

CREATE TABLE run_contexts (
    run_id TEXT PRIMARY KEY,
    role TEXT NOT NULL,
    name TEXT NOT NULL,
    parent_run_id TEXT,
    config_json TEXT NOT NULL,
    messages_json TEXT NOT NULL,
    checkpoints_json TEXT NOT NULL,
    terminal_phase TEXT NOT NULL,
    restorable INTEGER NOT NULL,
    updated_at_ns INTEGER NOT NULL
);
