-- Claims deliberately outlive temporary evidence snapshots: never replay an admitted pipeline.
CREATE TABLE learning_claims (
    task_id TEXT PRIMARY KEY NOT NULL,
    is_goal INTEGER NOT NULL,
    utc_day INTEGER NOT NULL
);
CREATE TABLE learning_claim_roots (
    task_id TEXT NOT NULL REFERENCES learning_claims(task_id),
    run_id TEXT NOT NULL,
    PRIMARY KEY(task_id, run_id)
);
CREATE INDEX learning_claims_day ON learning_claims(utc_day);
CREATE INDEX learning_claim_roots_run ON learning_claim_roots(run_id);
