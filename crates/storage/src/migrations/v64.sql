-- 07b-2b: rebuild the queue's state constraint without changing prior migrations.
DROP TRIGGER learning_job_forward_only;
DROP TRIGGER learning_job_started_keeps_origin;
DROP TRIGGER learning_job_no_delete;
DROP TRIGGER learning_job_evidence_immutable;
DROP TRIGGER learning_job_evidence_delete_after_end;
DROP INDEX learning_jobs_state;
ALTER TABLE learning_jobs RENAME TO learning_jobs_previous;
ALTER TABLE learning_job_evidence RENAME TO learning_job_evidence_previous;
CREATE TABLE learning_jobs (
    task_id TEXT PRIMARY KEY NOT NULL REFERENCES learning_claims(task_id),
    state TEXT NOT NULL CHECK (state IN ('Pending', 'Started', 'Interrupted', 'Finished', 'Failed', 'Abandoned')),
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
    attempt INTEGER NOT NULL DEFAULT 0,
    max_attempts INTEGER NOT NULL DEFAULT 1 CHECK(max_attempts >= 1),
    generation INTEGER NOT NULL DEFAULT 0,
    suspension_id TEXT,
    extraction_run TEXT,
    review_run TEXT,
    publication TEXT,
    reason TEXT
);
INSERT INTO learning_jobs(task_id,state,is_goal,project,source_run_id,origin_kind,origin_instance,origin_pid,origin_start_ticks,origin_boot_id,origin_pid_ns,config_identity,revision,admitted_at_ns,started_at_ns,finished_at_ns,reason) SELECT task_id,state,is_goal,project,source_run_id,origin_kind,origin_instance,origin_pid,origin_start_ticks,origin_boot_id,origin_pid_ns,config_identity,revision,admitted_at_ns,started_at_ns,finished_at_ns,reason FROM learning_jobs_previous;
CREATE INDEX learning_jobs_state ON learning_jobs(state, origin_kind);
CREATE TABLE learning_job_evidence (
    task_id TEXT PRIMARY KEY NOT NULL REFERENCES learning_jobs(task_id),
    prompt TEXT NOT NULL, evidence_json TEXT NOT NULL, sha256 TEXT NOT NULL
);
INSERT INTO learning_job_evidence SELECT * FROM learning_job_evidence_previous;
DROP TABLE learning_job_evidence_previous;
DROP TABLE learning_jobs_previous;
CREATE TABLE learning_job_staging (
    task_id TEXT PRIMARY KEY NOT NULL REFERENCES learning_jobs(task_id),
    lessons_json TEXT NOT NULL, reviews_json TEXT
);
CREATE TABLE learning_job_publications (
    task_id TEXT PRIMARY KEY NOT NULL REFERENCES learning_jobs(task_id),
    attempt INTEGER NOT NULL, outcome TEXT NOT NULL,
    reviews_json TEXT, committed_at_ns INTEGER NOT NULL
);
CREATE TRIGGER learning_job_forward_only BEFORE UPDATE ON learning_jobs
WHEN OLD.state IN ('Finished','Failed','Abandoned')
  OR (OLD.state = 'Started' AND NEW.state = 'Pending')
  OR NEW.task_id IS NOT OLD.task_id OR NEW.project IS NOT OLD.project
  OR NEW.source_run_id IS NOT OLD.source_run_id OR NEW.is_goal IS NOT OLD.is_goal
  OR NEW.admitted_at_ns IS NOT OLD.admitted_at_ns OR NEW.origin_kind IS NOT OLD.origin_kind
  OR NEW.max_attempts IS NOT OLD.max_attempts
  OR NEW.attempt < OLD.attempt OR NEW.generation < OLD.generation
  OR NEW.revision <= OLD.revision
BEGIN SELECT RAISE(ABORT, 'learning job transition is not allowed'); END;
CREATE TRIGGER learning_job_no_delete BEFORE DELETE ON learning_jobs
BEGIN SELECT RAISE(ABORT, 'learning jobs are kept'); END;
CREATE TRIGGER learning_job_evidence_immutable BEFORE UPDATE ON learning_job_evidence
BEGIN SELECT RAISE(ABORT, 'learning evidence is immutable'); END;
CREATE TRIGGER learning_job_evidence_delete_after_end BEFORE DELETE ON learning_job_evidence
WHEN (SELECT state FROM learning_jobs WHERE task_id=OLD.task_id) IN ('Pending','Started','Interrupted')
BEGIN SELECT RAISE(ABORT, 'learning evidence of a live job is immutable'); END;
CREATE TABLE learning_attempt_receipts (
    task_id TEXT NOT NULL REFERENCES learning_jobs(task_id), revision INTEGER NOT NULL,
    command_hash TEXT NOT NULL, result_key_json TEXT NOT NULL, PRIMARY KEY(task_id,revision)
);
CREATE TRIGGER learning_job_started_keeps_origin BEFORE UPDATE ON learning_jobs
WHEN OLD.state='Started' AND NEW.origin_instance IS NOT OLD.origin_instance
BEGIN SELECT RAISE(ABORT, 'a started learning job keeps its origin'); END;
