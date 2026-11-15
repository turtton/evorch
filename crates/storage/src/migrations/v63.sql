-- Durable learning queue (07b-2). A job is admitted together with its dedup claim
-- (learning_claims / learning_claim_roots) and immutable evidence in one transaction.
-- Pending -> Started (compare-and-set) -> Finished | Failed | Abandoned. Started and
-- terminal rows are never queued again; the evidence is deleted when the job ends.
CREATE TABLE learning_jobs (
    task_id TEXT PRIMARY KEY NOT NULL REFERENCES learning_claims(task_id),
    state TEXT NOT NULL CHECK (state IN ('Pending', 'Started', 'Finished', 'Failed', 'Abandoned')),
    is_goal INTEGER NOT NULL,
    project TEXT NOT NULL,
    source_run_id TEXT NOT NULL,
    origin_kind TEXT NOT NULL CHECK (origin_kind IN ('Core', 'Gui')),
    origin_instance TEXT NOT NULL,
    origin_pid INTEGER NOT NULL,
    origin_start_ticks INTEGER,
    origin_boot_id TEXT,
    origin_pid_ns TEXT,
    config_identity TEXT,
    revision INTEGER NOT NULL DEFAULT 0,
    admitted_at_ns INTEGER NOT NULL,
    started_at_ns INTEGER,
    finished_at_ns INTEGER,
    reason TEXT
);
CREATE INDEX learning_jobs_state ON learning_jobs(state, origin_kind);
CREATE TABLE learning_job_evidence (
    task_id TEXT PRIMARY KEY NOT NULL REFERENCES learning_jobs(task_id),
    prompt TEXT NOT NULL,
    evidence_json TEXT NOT NULL,
    sha256 TEXT NOT NULL
);
-- A job never moves backwards, and its identity columns never change.
CREATE TRIGGER learning_job_forward_only BEFORE UPDATE ON learning_jobs
WHEN OLD.state IN ('Finished', 'Failed', 'Abandoned')
  OR (OLD.state = 'Started' AND NEW.state = 'Pending')
  OR NEW.task_id IS NOT OLD.task_id
  OR NEW.is_goal IS NOT OLD.is_goal
  OR NEW.project IS NOT OLD.project
  OR NEW.source_run_id IS NOT OLD.source_run_id
  OR NEW.origin_kind IS NOT OLD.origin_kind
  OR NEW.admitted_at_ns IS NOT OLD.admitted_at_ns
BEGIN
    SELECT RAISE(ABORT, 'learning job transition is not allowed');
END;
CREATE TRIGGER learning_job_started_keeps_origin BEFORE UPDATE ON learning_jobs
WHEN OLD.state = 'Started' AND NEW.state = 'Started'
  AND (NEW.origin_instance IS NOT OLD.origin_instance)
BEGIN
    SELECT RAISE(ABORT, 'a started learning job keeps its origin');
END;
CREATE TRIGGER learning_job_no_delete BEFORE DELETE ON learning_jobs
BEGIN
    SELECT RAISE(ABORT, 'learning jobs are kept');
END;
-- Evidence is immutable while its job is live and removed only once the job ended.
CREATE TRIGGER learning_job_evidence_immutable BEFORE UPDATE ON learning_job_evidence
BEGIN
    SELECT RAISE(ABORT, 'learning evidence is immutable');
END;
CREATE TRIGGER learning_job_evidence_delete_after_end BEFORE DELETE ON learning_job_evidence
WHEN (SELECT state FROM learning_jobs WHERE task_id = OLD.task_id) IN ('Pending', 'Started')
BEGIN
    SELECT RAISE(ABORT, 'learning evidence of a live job is immutable');
END;
