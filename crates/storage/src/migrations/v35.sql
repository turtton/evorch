-- Archived-thread retention, storage foundation (PR-1).
--
-- Tombstones are the single irreversible boundary of a deletion: once a family
-- is confirmed, readers stop seeing it and every writer is rejected by the fence
-- triggers below, while rows are reclaimed in bounded steps. DELETE is never
-- fenced, so reclaim always makes progress.

-- 4.1 Archive and pin mirror columns. NULL revisions mean "never synchronised".
ALTER TABLE threads ADD COLUMN archived_at_ns INTEGER;
ALTER TABLE threads ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0 CHECK (pinned IN (0, 1));
ALTER TABLE threads ADD COLUMN archive_revision INTEGER;
ALTER TABLE threads ADD COLUMN pin_revision INTEGER;
CREATE INDEX idx_threads_archived ON threads(archived) WHERE archived = 1;
-- Closure probes: threads by recorded root run / escalation source run, tasks by
-- owning run, references by source run, and chat roots by the thread they name
-- (the exact expression of `runs_of_thread`).
CREATE INDEX idx_threads_root_run ON threads(root_run_id) WHERE root_run_id IS NOT NULL;
CREATE INDEX idx_threads_escalation_source ON threads(escalation_source_run_id)
    WHERE escalation_source_run_id IS NOT NULL;
CREATE INDEX idx_tasks_parent_run ON tasks(parent_run_id) WHERE parent_run_id IS NOT NULL;
CREATE INDEX idx_assistant_references_source ON assistant_references(source_run_id);
CREATE INDEX idx_run_contexts_chat_thread
    ON run_contexts(substr(name, 6 + instr(substr(name, 6), ':')))
    WHERE parent_run_id IS NULL AND substr(name, 1, 5) = 'chat:'
        AND instr(substr(name, 6), ':') > 1;

-- 4.2 Tombstones, staging and reclaim bookkeeping.
CREATE TABLE retention_identity (id INTEGER PRIMARY KEY CHECK (id = 1), db_id TEXT NOT NULL);
CREATE TABLE retention_binding (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    ownership_root TEXT NOT NULL,
    sidebar_path TEXT NOT NULL,
    bound_at_ns INTEGER NOT NULL
);

CREATE TABLE thread_tombstones (
    thread_id TEXT PRIMARY KEY,
    family_key TEXT NOT NULL,
    deleted_at_ns INTEGER NOT NULL
);
CREATE INDEX idx_thread_tombstones_family ON thread_tombstones(family_key, thread_id);
CREATE TABLE run_tombstones (
    run_id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL,
    family_key TEXT NOT NULL
);
CREATE INDEX idx_run_tombstones_family ON run_tombstones(family_key, run_id);
CREATE TABLE task_tombstones (
    task_id TEXT PRIMARY KEY,
    family_key TEXT NOT NULL
);
CREATE INDEX idx_task_tombstones_family ON task_tombstones(family_key, task_id);

CREATE TABLE retention_staging_state (
    family_key TEXT PRIMARY KEY,
    started_at_ns INTEGER NOT NULL,
    input_hash TEXT NOT NULL,
    explored_edges INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE retention_staging (
    family_key TEXT NOT NULL REFERENCES retention_staging_state(family_key) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('thread', 'run', 'frontier_thread', 'frontier_run')),
    id TEXT NOT NULL,
    -- Where a frontier item's expansion stopped: the edge query and its last rowid.
    edge_step INTEGER NOT NULL DEFAULT 0,
    edge_after INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (family_key, kind, id)
) WITHOUT ROWID;

-- Merging families and finding a member's family look members up by identity.
CREATE INDEX idx_retention_staging_member ON retention_staging(kind, id);

CREATE TABLE reclaim_jobs (
    family_key TEXT PRIMARY KEY,
    state TEXT NOT NULL CHECK (state IN
        ('db_pending', 'db_done', 'files_pending', 'done', 'files_failed')),
    cursor_json TEXT NOT NULL DEFAULT '{}',
    created_at_ns INTEGER NOT NULL,
    updated_at_ns INTEGER NOT NULL,
    estimate_bytes INTEGER NOT NULL DEFAULT 0,
    reclaimed_rows INTEGER NOT NULL DEFAULT 0,
    reclaimed_stored_bytes INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE reclaim_oversized (
    family_key TEXT NOT NULL,
    tbl TEXT NOT NULL,
    key TEXT NOT NULL,
    stored_bytes INTEGER NOT NULL,
    PRIMARY KEY (family_key, tbl, key)
) WITHOUT ROWID;

-- Append-only ledgers may only lose rows a reclaim step granted in the same
-- transaction.
CREATE TABLE run_ledger_delete_grants (run_id TEXT PRIMARY KEY);
CREATE TABLE task_queue_delete_grants (seq INTEGER PRIMARY KEY);

-- Used by the artifact collector (a later change); only the tables exist here.
CREATE TABLE artifact_refs (
    artifact_id TEXT PRIMARY KEY,
    sha256 TEXT NOT NULL,
    thread_id TEXT,
    run_id TEXT,
    recorded_at_ns INTEGER NOT NULL
);
CREATE INDEX idx_artifact_refs_sha ON artifact_refs(sha256);
CREATE TABLE gc_state (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    meta_cursor TEXT,
    blob_cursor TEXT,
    refs_complete INTEGER NOT NULL DEFAULT 0 CHECK (refs_complete IN (0, 1))
);
INSERT INTO gc_state(id) VALUES (1);
CREATE TABLE gc_unknown_meta (
    name TEXT PRIMARY KEY,
    first_seen_ns INTEGER NOT NULL,
    last_error TEXT NOT NULL
);

-- Orchestrator events that carry no run attribution until the second backfill
-- target fills it in.
CREATE INDEX idx_events_orchestrator_unattributed ON events(id)
    WHERE run_id IS NULL
        AND subkind IN ('TaskProgressed', 'TaskCheckpoint', 'ThreadGoalUpdated');

-- 4.3 Ledger deletes are allowed only with a grant.
DROP TRIGGER run_ledger_no_delete;
CREATE TRIGGER run_ledger_no_delete BEFORE DELETE ON run_ledger
WHEN NOT EXISTS (SELECT 1 FROM run_ledger_delete_grants WHERE run_id = OLD.run_id)
BEGIN SELECT RAISE(ABORT, 'run ledger is append-only'); END;
DROP TRIGGER task_queue_no_delete;
CREATE TRIGGER task_queue_no_delete BEFORE DELETE ON task_queue_ledger
WHEN NOT EXISTS (SELECT 1 FROM task_queue_delete_grants WHERE seq = OLD.seq)
BEGIN SELECT RAISE(ABORT, 'task queue ledger is append-only'); END;

-- 'archived' joins the eviction reasons: rebuild the table with the wider CHECK.
CREATE TABLE thread_index_runs_v35 (
    run_id TEXT PRIMARY KEY NOT NULL,
    state TEXT NOT NULL CHECK (state IN (
        'pending', 'indexed', 'retry_wait', 'blocked', 'evicted', 'deleted')),
    source_revision INTEGER NOT NULL CHECK (source_revision >= 0),
    snapshot_revision INTEGER NOT NULL CHECK (snapshot_revision >= 0),
    indexed_revision INTEGER,
    source_context_sha256 TEXT NOT NULL,
    source_note_seq INTEGER,
    source_provenance_version INTEGER,
    extractor_version INTEGER NOT NULL,
    accounting_version INTEGER NOT NULL,
    indexed_extractor_version INTEGER,
    indexed_accounting_version INTEGER,
    source_updated_at_ns INTEGER NOT NULL,
    retention_at_ns INTEGER NOT NULL,
    indexed_at_ns INTEGER,
    text_bytes INTEGER NOT NULL DEFAULT 0 CHECK (text_bytes >= 0),
    attribute_bytes INTEGER NOT NULL DEFAULT 0 CHECK (attribute_bytes >= 0),
    block_count INTEGER NOT NULL DEFAULT 0 CHECK (block_count >= 0),
    charged_bytes INTEGER NOT NULL DEFAULT 0 CHECK (charged_bytes >= 0),
    priority INTEGER NOT NULL DEFAULT 0,
    attempt_count INTEGER NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
    next_attempt_at_ns INTEGER,
    blocked_reason TEXT,
    last_error_code TEXT,
    evicted_reason TEXT CHECK (evicted_reason IN (
        'logical_budget', 'oversized', 'physical_reclaim', 'archived')),
    work_generation INTEGER NOT NULL DEFAULT 0 CHECK (work_generation >= 0),
    source_present INTEGER NOT NULL DEFAULT 1 CHECK (source_present IN (0, 1)),
    source_after_tombstone INTEGER NOT NULL DEFAULT 0 CHECK (source_after_tombstone IN (0, 1)),
    CHECK ((state = 'evicted') = (evicted_reason IS NOT NULL)),
    CHECK ((state = 'blocked') = (blocked_reason IS NOT NULL)),
    CHECK ((state = 'retry_wait') = (next_attempt_at_ns IS NOT NULL)),
    CHECK (state <> 'deleted' OR source_present = 0)
);
INSERT INTO thread_index_runs_v35 SELECT * FROM thread_index_runs;
DROP TABLE thread_index_runs;
ALTER TABLE thread_index_runs_v35 RENAME TO thread_index_runs;
CREATE INDEX idx_thread_index_runs_work
    ON thread_index_runs(state, priority DESC, retention_at_ns DESC, run_id DESC);
CREATE INDEX idx_thread_index_runs_retention
    ON thread_index_runs(retention_at_ns, run_id) WHERE charged_bytes > 0;
CREATE INDEX idx_thread_index_runs_retry
    ON thread_index_runs(next_attempt_at_ns) WHERE state = 'retry_wait';

-- Fence triggers: nothing may be inserted for, or moved back to, a tombstoned
-- identity. DELETE is deliberately unfenced. RAISE(IGNORE) is never used.
CREATE TRIGGER fence_run_contexts_insert BEFORE INSERT ON run_contexts
WHEN NEW.run_id IN (SELECT run_id FROM run_tombstones) OR NEW.parent_run_id IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
CREATE TRIGGER fence_run_contexts_update BEFORE UPDATE ON run_contexts
WHEN NEW.run_id IN (SELECT run_id FROM run_tombstones) OR NEW.parent_run_id IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
CREATE TRIGGER fence_run_context_provenance_insert BEFORE INSERT ON run_context_provenance
WHEN NEW.run_id IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
CREATE TRIGGER fence_run_context_provenance_update BEFORE UPDATE ON run_context_provenance
WHEN NEW.run_id IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
CREATE TRIGGER fence_run_ledger_insert BEFORE INSERT ON run_ledger
WHEN NEW.run_id IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
CREATE TRIGGER fence_host_input_receipts_insert BEFORE INSERT ON host_input_receipts
WHEN NEW.run_id IN (SELECT run_id FROM run_tombstones) OR NEW.root_run_id IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
CREATE TRIGGER fence_host_input_receipts_update BEFORE UPDATE ON host_input_receipts
WHEN NEW.run_id IN (SELECT run_id FROM run_tombstones) OR NEW.root_run_id IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
CREATE TRIGGER fence_review_evidence_insert BEFORE INSERT ON review_evidence
WHEN NEW.root_run_id IN (SELECT run_id FROM run_tombstones) OR NEW.target_run_id IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
CREATE TRIGGER fence_review_evidence_update BEFORE UPDATE ON review_evidence
WHEN NEW.root_run_id IN (SELECT run_id FROM run_tombstones) OR NEW.target_run_id IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
CREATE TRIGGER fence_review_lineage_edges_insert BEFORE INSERT ON review_lineage_edges
WHEN NEW.child_run_id IN (SELECT run_id FROM run_tombstones) OR NEW.parent_run_id IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
CREATE TRIGGER fence_review_lineage_edges_update BEFORE UPDATE ON review_lineage_edges
WHEN NEW.child_run_id IN (SELECT run_id FROM run_tombstones) OR NEW.parent_run_id IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
CREATE TRIGGER fence_assistant_references_insert BEFORE INSERT ON assistant_references
WHEN NEW.source_run_id IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
CREATE TRIGGER fence_assistant_references_update BEFORE UPDATE ON assistant_references
WHEN NEW.source_run_id IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
CREATE TRIGGER fence_user_questions_insert BEFORE INSERT ON user_questions
WHEN NEW.run_id IN (SELECT run_id FROM run_tombstones) OR NEW.root_run_id IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
CREATE TRIGGER fence_user_questions_update BEFORE UPDATE ON user_questions
WHEN NEW.run_id IN (SELECT run_id FROM run_tombstones) OR NEW.root_run_id IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
CREATE TRIGGER fence_user_question_links_insert BEFORE INSERT ON user_question_links
WHEN NEW.run_id IN (SELECT run_id FROM run_tombstones)
    OR EXISTS (SELECT 1 FROM user_questions q WHERE q.id = NEW.question_id
        AND (q.run_id IN (SELECT run_id FROM run_tombstones)
            OR q.root_run_id IN (SELECT run_id FROM run_tombstones)))
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
CREATE TRIGGER fence_user_question_links_update BEFORE UPDATE ON user_question_links
WHEN NEW.run_id IN (SELECT run_id FROM run_tombstones)
    OR EXISTS (SELECT 1 FROM user_questions q WHERE q.id = NEW.question_id
        AND (q.run_id IN (SELECT run_id FROM run_tombstones)
            OR q.root_run_id IN (SELECT run_id FROM run_tombstones)))
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
CREATE TRIGGER fence_conversation_blocks_insert BEFORE INSERT ON conversation_blocks
WHEN NEW.run_id IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
CREATE TRIGGER fence_conversation_blocks_update BEFORE UPDATE ON conversation_blocks
WHEN NEW.run_id IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
CREATE TRIGGER fence_thread_runs_insert BEFORE INSERT ON thread_runs
WHEN NEW.thread_id IN (SELECT thread_id FROM thread_tombstones) OR NEW.run_id IN (SELECT run_id FROM run_tombstones) OR NEW.root_run_id IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
CREATE TRIGGER fence_thread_runs_update BEFORE UPDATE ON thread_runs
WHEN NEW.thread_id IN (SELECT thread_id FROM thread_tombstones) OR NEW.run_id IN (SELECT run_id FROM run_tombstones) OR NEW.root_run_id IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
CREATE TRIGGER fence_thread_start_states_insert BEFORE INSERT ON thread_start_states
WHEN NEW.thread_id IN (SELECT thread_id FROM thread_tombstones) OR NEW.root_run_id IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
CREATE TRIGGER fence_thread_start_states_update BEFORE UPDATE ON thread_start_states
WHEN NEW.thread_id IN (SELECT thread_id FROM thread_tombstones) OR NEW.root_run_id IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
CREATE TRIGGER fence_thread_start_untracked_insert BEFORE INSERT ON thread_start_untracked
WHEN NEW.thread_id IN (SELECT thread_id FROM thread_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
CREATE TRIGGER fence_thread_start_untracked_update BEFORE UPDATE ON thread_start_untracked
WHEN NEW.thread_id IN (SELECT thread_id FROM thread_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
CREATE TRIGGER fence_eval_samples_insert BEFORE INSERT ON eval_samples
WHEN NEW.thread_id IN (SELECT thread_id FROM thread_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
CREATE TRIGGER fence_eval_samples_update BEFORE UPDATE ON eval_samples
WHEN NEW.thread_id IN (SELECT thread_id FROM thread_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
CREATE TRIGGER fence_usage_run_threads_insert BEFORE INSERT ON usage_run_threads
WHEN NEW.thread_id IN (SELECT thread_id FROM thread_tombstones) OR NEW.run_id IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
CREATE TRIGGER fence_usage_run_threads_update BEFORE UPDATE ON usage_run_threads
WHEN NEW.thread_id IN (SELECT thread_id FROM thread_tombstones) OR NEW.run_id IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
CREATE TRIGGER fence_threads_insert BEFORE INSERT ON threads
WHEN NEW.thread_id IN (SELECT thread_id FROM thread_tombstones) OR NEW.parent_thread_id IN (SELECT thread_id FROM thread_tombstones) OR NEW.escalation_source_run_id IN (SELECT run_id FROM run_tombstones) OR NEW.root_run_id IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
CREATE TRIGGER fence_threads_update BEFORE UPDATE ON threads
WHEN NEW.thread_id IN (SELECT thread_id FROM thread_tombstones) OR NEW.parent_thread_id IN (SELECT thread_id FROM thread_tombstones) OR NEW.escalation_source_run_id IN (SELECT run_id FROM run_tombstones) OR NEW.root_run_id IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
CREATE TRIGGER fence_tasks_insert BEFORE INSERT ON tasks
WHEN NEW.id IN (SELECT task_id FROM task_tombstones) OR NEW.parent_run_id IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT, 'task is tombstoned'); END;
CREATE TRIGGER fence_tasks_update BEFORE UPDATE ON tasks
WHEN NEW.id IN (SELECT task_id FROM task_tombstones) OR NEW.parent_run_id IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT, 'task is tombstoned'); END;
CREATE TRIGGER fence_task_links_insert BEFORE INSERT ON task_links
WHEN NEW.blocker_id IN (SELECT task_id FROM task_tombstones) OR NEW.blocked_id IN (SELECT task_id FROM task_tombstones)
BEGIN SELECT RAISE(ABORT, 'task is tombstoned'); END;

CREATE TRIGGER fence_thread_index_runs_insert BEFORE INSERT ON thread_index_runs
WHEN NEW.run_id IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
-- A reclaim step may still terminalise the row ('deleted' / 'evicted').
CREATE TRIGGER fence_thread_index_runs_update BEFORE UPDATE ON thread_index_runs
WHEN NEW.run_id IN (SELECT run_id FROM run_tombstones) AND NEW.state NOT IN ('deleted', 'evicted')
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
CREATE TRIGGER fence_task_queue_ledger_insert BEFORE INSERT ON task_queue_ledger
WHEN json_valid(NEW.payload) AND (
    (json_extract(NEW.payload, '$.id')) IN (SELECT task_id FROM task_tombstones)
    OR (json_extract(NEW.payload, '$.blocker')) IN (SELECT task_id FROM task_tombstones)
    OR (json_extract(NEW.payload, '$.blocked')) IN (SELECT task_id FROM task_tombstones))
BEGIN SELECT RAISE(ABORT, 'task is tombstoned'); END;
-- Backstop for the writer's own check: attributed events of a tombstoned run.
-- Thread-scoped Orchestrator snapshots are rejected by the writer (Rust side):
-- the payload column must not be read while appending (see accounting tests).
CREATE TRIGGER fence_events_insert BEFORE INSERT ON events
WHEN NEW.run_id IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
