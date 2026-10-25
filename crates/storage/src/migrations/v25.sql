-- Conversation full-text index (ADR 0041 Phase B, M3 / R1 / R2).
--
-- Tables, indexes and triggers only: no message body is read or rewritten here.
-- Existing snapshots get snapshot_revision 0 and no provenance sidecar, so they
-- are not verified by migration; discovery requires structured generation proof.
ALTER TABLE run_contexts ADD COLUMN snapshot_revision INTEGER NOT NULL DEFAULT 0
    CHECK (snapshot_revision >= 0);

-- One sidecar row describing the current redacted snapshot of a run. Positions
-- and copy origins only; text and secrets are never duplicated here. Revision 0
-- is reserved for an explicitly derived legacy snapshot, not a new save.
CREATE TABLE run_context_provenance (
    run_id TEXT PRIMARY KEY NOT NULL,
    snapshot_revision INTEGER NOT NULL CHECK (snapshot_revision >= 0),
    snapshot_sha256 TEXT NOT NULL,
    format_version INTEGER NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('verified', 'unknown')),
    reason_code TEXT CHECK (reason_code IN (
        'missing_sidecar', 'snapshot_mismatch', 'incomplete_map', 'legacy_unproven',
        'unsupported_version')),
    evidence_json TEXT,
    map_json TEXT NOT NULL,
    CHECK ((status = 'unknown') = (reason_code IS NOT NULL)),
    CHECK ((status = 'verified') = (evidence_json IS NOT NULL))
);

-- Per-run index state (R2). Source columns describe the latest canonical input;
-- indexed_* columns describe what conversation_blocks currently holds.
CREATE TABLE thread_index_runs (
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
        'logical_budget', 'oversized', 'physical_reclaim')),
    work_generation INTEGER NOT NULL DEFAULT 0 CHECK (work_generation >= 0),
    source_present INTEGER NOT NULL DEFAULT 1 CHECK (source_present IN (0, 1)),
    source_after_tombstone INTEGER NOT NULL DEFAULT 0 CHECK (source_after_tombstone IN (0, 1)),
    CHECK ((state = 'evicted') = (evicted_reason IS NOT NULL)),
    CHECK ((state = 'blocked') = (blocked_reason IS NOT NULL)),
    CHECK ((state = 'retry_wait') = (next_attempt_at_ns IS NOT NULL)),
    CHECK (state <> 'deleted' OR source_present = 0)
);
CREATE INDEX idx_thread_index_runs_work
    ON thread_index_runs(state, priority DESC, retention_at_ns DESC, run_id DESC);
-- Eviction order (B-4). Pending rows may still hold the previous revision's
-- blocks until their replacement commits, so they are candidates too.
CREATE INDEX idx_thread_index_runs_retention
    ON thread_index_runs(retention_at_ns, run_id) WHERE charged_bytes > 0;
CREATE INDEX idx_thread_index_runs_retry
    ON thread_index_runs(next_attempt_at_ns) WHERE state = 'retry_wait';

-- Extracted, redacted, truncated text. source_key is the stable position key
-- ("<ordinal>:<block_index>" for messages, "working_note" for the latest note);
-- writes use ON CONFLICT DO UPDATE, never INSERT OR REPLACE.
CREATE TABLE conversation_blocks (
    id INTEGER PRIMARY KEY,
    run_id TEXT NOT NULL,
    source_kind TEXT NOT NULL CHECK (source_kind IN ('message', 'working_note')),
    source_key TEXT NOT NULL,
    ordinal INTEGER CHECK (ordinal >= 0),
    block_index INTEGER CHECK (block_index >= 0),
    note_seq INTEGER CHECK (note_seq >= 1),
    role TEXT NOT NULL CHECK (role IN ('user', 'assistant', 'working_note')),
    kind TEXT NOT NULL CHECK (kind IN (
        'text', 'working_note', 'tool_use', 'tool_result', 'tool_error')),
    tool_name TEXT,
    truncated INTEGER NOT NULL CHECK (truncated IN (0, 1)),
    hash TEXT NOT NULL,
    text TEXT NOT NULL,
    UNIQUE (run_id, source_kind, source_key),
    CHECK ((source_kind = 'message')
        = (ordinal IS NOT NULL AND block_index IS NOT NULL AND note_seq IS NULL)),
    CHECK ((source_kind = 'working_note')
        = (note_seq IS NOT NULL AND ordinal IS NULL AND block_index IS NULL))
);
CREATE INDEX idx_conversation_blocks_kind ON conversation_blocks(kind, run_id);

CREATE VIRTUAL TABLE conversation_fts USING fts5(
    text, content = conversation_blocks, content_rowid = id,
    tokenize = 'trigram', columnsize = 0
);
CREATE TRIGGER conversation_blocks_insert AFTER INSERT ON conversation_blocks BEGIN
    INSERT INTO conversation_fts(rowid, text) VALUES(new.id, new.text);
END;
-- Locator-only updates (same hash, new position) do not touch the FTS index.
CREATE TRIGGER conversation_blocks_update AFTER UPDATE OF text ON conversation_blocks
WHEN old.text IS NOT new.text BEGIN
    INSERT INTO conversation_fts(conversation_fts, rowid, text) VALUES('delete', old.id, old.text);
    INSERT INTO conversation_fts(rowid, text) VALUES(new.id, new.text);
END;
CREATE TRIGGER conversation_blocks_delete AFTER DELETE ON conversation_blocks BEGIN
    INSERT INTO conversation_fts(conversation_fts, rowid, text) VALUES('delete', old.id, old.text);
END;

-- Singleton: applied extractor/accounting versions for new rows, and where the
-- header discovery scan resumes (newest runs first).
CREATE TABLE thread_index_state (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    extractor_version INTEGER NOT NULL,
    accounting_version INTEGER NOT NULL,
    discovery_retention_at_ns INTEGER,
    discovery_run_id TEXT,
    discovery_completed_at_ns INTEGER,
    CHECK ((discovery_retention_at_ns IS NULL) = (discovery_run_id IS NULL))
);
INSERT INTO thread_index_state(id, extractor_version, accounting_version) VALUES (1, 1, 0);

-- DB-scoped lease with a fencing epoch; the epoch only ever increases.
CREATE TABLE thread_index_lease (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    owner_id TEXT,
    owner_kind TEXT CHECK (owner_kind IN ('gui', 'cli')),
    epoch INTEGER NOT NULL CHECK (epoch >= 0),
    expires_at_ns INTEGER,
    CHECK ((owner_id IS NULL) = (owner_kind IS NULL)),
    CHECK ((owner_id IS NULL) = (expires_at_ns IS NULL))
);
INSERT INTO thread_index_lease(id, epoch) VALUES (1, 0);
CREATE TRIGGER thread_index_lease_epoch_monotonic BEFORE UPDATE OF epoch ON thread_index_lease
WHEN new.epoch < old.epoch BEGIN
    SELECT RAISE(ABORT, 'thread index lease epoch must not decrease');
END;

CREATE TABLE thread_index_requests (
    request_id TEXT PRIMARY KEY NOT NULL,
    canonical_project TEXT,
    since_ns INTEGER,
    include_evicted INTEGER NOT NULL CHECK (include_evicted IN (0, 1)),
    state TEXT NOT NULL CHECK (state IN (
        'pending', 'completed', 'completed_with_gaps', 'failed', 'cancelled')),
    failure_reason TEXT,
    deleted_before_submission INTEGER NOT NULL DEFAULT 0
        CHECK (deleted_before_submission >= 0),
    created_at_ns INTEGER NOT NULL,
    finished_at_ns INTEGER,
    CHECK ((state = 'failed') = (failure_reason IS NOT NULL)),
    CHECK ((state = 'pending') = (finished_at_ns IS NULL))
);

-- Targets freeze (run, revision, versions) at submission and are never rewritten;
-- only work_generation (shared-work rebinding) and the outcome may change.
CREATE TABLE thread_index_request_targets (
    request_id TEXT NOT NULL,
    run_id TEXT NOT NULL,
    source_revision INTEGER NOT NULL,
    extractor_version INTEGER NOT NULL,
    accounting_version INTEGER NOT NULL,
    work_generation INTEGER NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('pending', 'succeeded', 'gap')),
    gap_reason TEXT,
    PRIMARY KEY (request_id, run_id),
    CHECK ((state = 'gap') = (gap_reason IS NOT NULL))
) WITHOUT ROWID;
CREATE INDEX idx_thread_index_request_targets_run
    ON thread_index_request_targets(run_id, state);
CREATE TRIGGER thread_index_request_targets_immutable
BEFORE UPDATE OF request_id, run_id, source_revision, extractor_version, accounting_version
ON thread_index_request_targets BEGIN
    SELECT RAISE(ABORT, 'thread index request targets are immutable');
END;
