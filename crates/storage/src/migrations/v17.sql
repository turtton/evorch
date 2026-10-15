CREATE TABLE review_evidence (
    id TEXT PRIMARY KEY,
    root_run_id TEXT NOT NULL,
    target_run_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    text TEXT NOT NULL,
    in_reply_to_question_id TEXT,
    in_reply_to_title TEXT,
    reference_kind TEXT,
    reference_context TEXT,
    created_at_ns INTEGER NOT NULL
);
CREATE UNIQUE INDEX review_evidence_id ON review_evidence(id);
CREATE INDEX review_evidence_target ON review_evidence(target_run_id, created_at_ns);
CREATE INDEX review_evidence_root ON review_evidence(root_run_id, created_at_ns);

CREATE TABLE review_lineage_edges (
    child_run_id TEXT NOT NULL,
    parent_run_id TEXT NOT NULL,
    created_at_ns INTEGER NOT NULL,
    PRIMARY KEY(child_run_id, parent_run_id)
);
CREATE INDEX review_lineage_edges_parent ON review_lineage_edges(parent_run_id);
