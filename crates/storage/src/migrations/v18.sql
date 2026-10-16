CREATE TABLE assistant_references (
    reference_id TEXT PRIMARY KEY,
    source_run_id TEXT NOT NULL,
    turn_end INTEGER NOT NULL CHECK (turn_end >= 0),
    text TEXT NOT NULL,
    consumed INTEGER NOT NULL CHECK (consumed IN (0, 1)),
    created_at_ns INTEGER NOT NULL
);
