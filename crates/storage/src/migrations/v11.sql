CREATE TABLE improvement_candidates (
    candidate_id TEXT PRIMARY KEY,
    project TEXT NOT NULL,
    created_at_ns INTEGER NOT NULL,
    source TEXT NOT NULL,
    code TEXT NOT NULL,
    severity TEXT NOT NULL,
    title TEXT NOT NULL,
    evidence TEXT NOT NULL,
    dedup_key TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'new',
    draft_path TEXT,
    run_id TEXT
);
CREATE INDEX idx_improvement_candidates_dedup ON improvement_candidates(project, dedup_key, created_at_ns);
CREATE INDEX idx_improvement_candidates_status ON improvement_candidates(project, status, created_at_ns);

-- Intake accounting survives candidate retention; only successful stores append rows.
CREATE TABLE improvement_intake (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    project TEXT NOT NULL,
    dedup_key TEXT NOT NULL,
    created_at_ns INTEGER NOT NULL,
    outcome TEXT NOT NULL,
    candidate_id TEXT NOT NULL
);
CREATE INDEX idx_improvement_intake_dedup ON improvement_intake(project, dedup_key, seq);
CREATE INDEX idx_improvement_intake_daily ON improvement_intake(project, created_at_ns);
