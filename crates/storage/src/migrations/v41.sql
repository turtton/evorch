-- Lane ledger (ADR 0044 Phase 1b, persistent contract).
--
-- A lane is an independent Orchestrator thread opened by a lead. The ledger keeps what
-- the thread row cannot: who opened it (immutable), who manages it now (mutable, with
-- a revision), its brief, its policy, the reports it made and how to start it again.
-- `threads.parent_thread_id` is NOT the management relation: a host mirror can only
-- set it (a NULL never erases a stored value), so a detach written there would be lost.

-- 1. `threads.lineage_kind` learns 'lane'. SQLite cannot change a CHECK in place, so
-- the table is rebuilt with the same columns, indexes and fence triggers.
CREATE TABLE threads_v41 (
    thread_id TEXT PRIMARY KEY,
    project_id TEXT,
    project_root TEXT,
    title TEXT,
    root_run_id TEXT,
    chat_role TEXT,
    parent_thread_id TEXT,
    lineage_kind TEXT CHECK (lineage_kind IN ('fork', 'rewind', 'escalation', 'lane')),
    fork_point_run_id TEXT,
    fork_point_context_len INTEGER CHECK (fork_point_context_len >= 0),
    escalation_source_run_id TEXT,
    archived INTEGER NOT NULL DEFAULT 0 CHECK (archived IN (0, 1)),
    superseded INTEGER NOT NULL DEFAULT 0 CHECK (superseded IN (0, 1)),
    created_at_ns INTEGER NOT NULL,
    updated_at_ns INTEGER NOT NULL,
    source TEXT NOT NULL CHECK (source IN ('runtime', 'host', 'backfill')),
    archived_at_ns INTEGER,
    pinned INTEGER NOT NULL DEFAULT 0 CHECK (pinned IN (0, 1)),
    archive_revision INTEGER,
    pin_revision INTEGER
);
INSERT INTO threads_v41 (
    thread_id, project_id, project_root, title, root_run_id, chat_role, parent_thread_id,
    lineage_kind, fork_point_run_id, fork_point_context_len, escalation_source_run_id,
    archived, superseded, created_at_ns, updated_at_ns, source, archived_at_ns, pinned,
    archive_revision, pin_revision
) SELECT
    thread_id, project_id, project_root, title, root_run_id, chat_role, parent_thread_id,
    lineage_kind, fork_point_run_id, fork_point_context_len, escalation_source_run_id,
    archived, superseded, created_at_ns, updated_at_ns, source, archived_at_ns, pinned,
    archive_revision, pin_revision
FROM threads;
DROP TABLE threads;
ALTER TABLE threads_v41 RENAME TO threads;
CREATE INDEX idx_threads_project_updated ON threads(project_root, updated_at_ns DESC);
CREATE INDEX idx_threads_parent ON threads(parent_thread_id);
CREATE INDEX idx_threads_archived ON threads(archived) WHERE archived = 1;
CREATE INDEX idx_threads_root_run ON threads(root_run_id) WHERE root_run_id IS NOT NULL;
CREATE INDEX idx_threads_escalation_source ON threads(escalation_source_run_id)
    WHERE escalation_source_run_id IS NOT NULL;
CREATE INDEX idx_threads_archived_by_id ON threads(thread_id) WHERE archived = 1;
CREATE TRIGGER fence_threads_insert BEFORE INSERT ON threads
WHEN NEW.thread_id IN (SELECT thread_id FROM thread_tombstones) OR NEW.parent_thread_id IN (SELECT thread_id FROM thread_tombstones) OR NEW.escalation_source_run_id IN (SELECT run_id FROM run_tombstones) OR NEW.root_run_id IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
CREATE TRIGGER fence_threads_update BEFORE UPDATE ON threads
WHEN NEW.thread_id IN (SELECT thread_id FROM thread_tombstones) OR NEW.parent_thread_id IN (SELECT thread_id FROM thread_tombstones) OR NEW.escalation_source_run_id IN (SELECT run_id FROM run_tombstones) OR NEW.root_run_id IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
-- A lane thread stays a lane thread: a fork or rewind label written over it by a
-- stale mirror is refused here, whatever the writer.
CREATE TRIGGER threads_lane_lineage_sticky BEFORE UPDATE OF lineage_kind ON threads
WHEN OLD.lineage_kind = 'lane' AND NEW.lineage_kind IS NOT 'lane'
BEGIN SELECT RAISE(ABORT, 'a lane thread keeps its lineage kind'); END;

-- 2. The ledger-wide revision. Every ledger mutation increments it, so it names a
-- state of the whole ledger; reads, monitor ticks and redraws never touch it.
CREATE TABLE lane_state (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    revision INTEGER NOT NULL CHECK (revision >= 0)
);
INSERT INTO lane_state(id, revision) VALUES (1, 0);

-- 3. One row per lane.
--   lane_id, thread_id, origin_*, creation_key, request_hash, scope: immutable.
--   lead_thread_id / policy / management_revision: the current management relation.
--   A detached lane has no lead; the origin stays.
--   generation 0 means the root was never started (the creation intent is durable and
--   `start_seed` says how to start it); every start or restart raises it by one.
CREATE TABLE lanes (
    lane_id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL UNIQUE,
    origin_thread_id TEXT NOT NULL,
    origin_run_id TEXT NOT NULL,
    creation_key TEXT NOT NULL,
    request_hash TEXT NOT NULL,
    scope TEXT NOT NULL CHECK (scope IN ('lane')),
    lead_thread_id TEXT,
    policy TEXT NOT NULL CHECK (policy IN ('attached', 'summary', 'detached')),
    management_revision INTEGER NOT NULL CHECK (management_revision >= 1),
    brief_json TEXT NOT NULL,
    brief_revision INTEGER NOT NULL CHECK (brief_revision >= 1),
    start_seed TEXT NOT NULL,
    root_run_id TEXT,
    generation INTEGER NOT NULL DEFAULT 0 CHECK (generation >= 0),
    latest_report_seq INTEGER NOT NULL DEFAULT 0 CHECK (latest_report_seq >= 0),
    created_at_ns INTEGER NOT NULL,
    updated_at_ns INTEGER NOT NULL,
    last_activity_at_ns INTEGER NOT NULL,
    CHECK ((policy = 'detached') = (lead_thread_id IS NULL)),
    CHECK ((generation = 0) = (root_run_id IS NULL)),
    UNIQUE (origin_thread_id, creation_key)
);
-- `lane_list`: the lanes a lead manages, newest real activity first, ties by lane ID.
CREATE INDEX idx_lanes_lead_activity
    ON lanes(lead_thread_id, last_activity_at_ns DESC, lane_id)
    WHERE lead_thread_id IS NOT NULL;
CREATE INDEX idx_lanes_origin ON lanes(origin_thread_id);
-- Creation intents whose root never started (repair after a crash).
CREATE INDEX idx_lanes_unstarted ON lanes(lane_id) WHERE generation = 0;

-- The origin, the scope and the idempotency identity never change, and the counters
-- never go back: a detach, a stale writer or a bug cannot erase where a lane came from.
CREATE TRIGGER lanes_immutable BEFORE UPDATE ON lanes
WHEN NEW.lane_id <> OLD.lane_id OR NEW.thread_id <> OLD.thread_id
    OR NEW.origin_thread_id <> OLD.origin_thread_id OR NEW.origin_run_id <> OLD.origin_run_id
    OR NEW.creation_key <> OLD.creation_key OR NEW.request_hash <> OLD.request_hash
    OR NEW.scope <> OLD.scope OR NEW.created_at_ns <> OLD.created_at_ns
    OR NEW.start_seed <> OLD.start_seed
BEGIN SELECT RAISE(ABORT, 'a lane keeps its origin, scope and creation identity'); END;
CREATE TRIGGER lanes_monotonic BEFORE UPDATE ON lanes
WHEN NEW.management_revision < OLD.management_revision OR NEW.brief_revision < OLD.brief_revision
    OR NEW.generation < OLD.generation OR NEW.latest_report_seq < OLD.latest_report_seq
    OR NEW.last_activity_at_ns < OLD.last_activity_at_ns
BEGIN SELECT RAISE(ABORT, 'lane counters never go back'); END;

CREATE TRIGGER fence_lanes_insert BEFORE INSERT ON lanes
WHEN NEW.thread_id IN (SELECT thread_id FROM thread_tombstones)
    OR NEW.origin_thread_id IN (SELECT thread_id FROM thread_tombstones)
    OR NEW.lead_thread_id IN (SELECT thread_id FROM thread_tombstones)
    OR NEW.origin_run_id IN (SELECT run_id FROM run_tombstones)
    OR NEW.root_run_id IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
-- An update is fenced by the lane's own thread and root run, and by a lead it moves
-- to. A lead that was deleted later does not freeze a lane that outlived it.
CREATE TRIGGER fence_lanes_update BEFORE UPDATE ON lanes
WHEN NEW.thread_id IN (SELECT thread_id FROM thread_tombstones)
    OR NEW.root_run_id IN (SELECT run_id FROM run_tombstones)
    OR (NEW.lead_thread_id IS NOT OLD.lead_thread_id
        AND NEW.lead_thread_id IN (SELECT thread_id FROM thread_tombstones))
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;

-- 4. Report history: runtime-numbered, dense, immutable.
CREATE TABLE lane_reports (
    lane_id TEXT NOT NULL REFERENCES lanes(lane_id) ON DELETE CASCADE,
    report_seq INTEGER NOT NULL CHECK (report_seq >= 1),
    status TEXT NOT NULL CHECK (status IN ('working', 'waiting', 'blocked', 'ready', 'failed')),
    summary TEXT NOT NULL,
    artifact_refs_json TEXT NOT NULL,
    needs_human INTEGER NOT NULL CHECK (needs_human IN (0, 1)),
    question_refs_json TEXT NOT NULL,
    reason_code TEXT,
    producer TEXT NOT NULL CHECK (producer IN ('lane_agent', 'harness')),
    management_revision INTEGER NOT NULL,
    brief_revision INTEGER NOT NULL,
    created_at_ns INTEGER NOT NULL,
    PRIMARY KEY (lane_id, report_seq)
) WITHOUT ROWID;
CREATE TRIGGER lane_reports_immutable BEFORE UPDATE ON lane_reports
BEGIN SELECT RAISE(ABORT, 'lane reports are immutable'); END;
-- The next report is exactly one past the lane's latest: no gaps, no reuse.
CREATE TRIGGER lane_reports_dense BEFORE INSERT ON lane_reports
WHEN NEW.report_seq <> (SELECT latest_report_seq + 1 FROM lanes WHERE lane_id = NEW.lane_id)
BEGIN SELECT RAISE(ABORT, 'lane report sequence is not the next one'); END;
CREATE TRIGGER fence_lane_reports_insert BEFORE INSERT ON lane_reports
WHEN EXISTS (SELECT 1 FROM lanes WHERE lane_id = NEW.lane_id
    AND thread_id IN (SELECT thread_id FROM thread_tombstones))
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;

-- 5. Vessel for Phase 1c delivery: the semantic key of a notification (lane,
-- management revision, event kind, and the question / artifact revision / failure
-- episode it is about). Registering a key twice is a no-op, so a resend never becomes
-- a second event. The outbox, claims and ACKs are added by the delivery change.
CREATE TABLE lane_delivery_keys (
    lane_id TEXT NOT NULL REFERENCES lanes(lane_id) ON DELETE CASCADE,
    management_revision INTEGER NOT NULL CHECK (management_revision >= 1),
    event_kind TEXT NOT NULL,
    dedupe_key TEXT NOT NULL,
    ledger_revision INTEGER NOT NULL,
    created_at_ns INTEGER NOT NULL,
    PRIMARY KEY (lane_id, management_revision, event_kind, dedupe_key)
) WITHOUT ROWID;
CREATE TRIGGER fence_lane_delivery_keys_insert BEFORE INSERT ON lane_delivery_keys
WHEN EXISTS (SELECT 1 FROM lanes WHERE lane_id = NEW.lane_id
    AND thread_id IN (SELECT thread_id FROM thread_tombstones))
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;

-- 6. `lane_list` snapshots. The first page of a result that does not fit one page
-- freezes the order (lane IDs only) so every later page continues the same list.
-- Snapshots are bounded by count (see repo/lanes.rs), not by time; an evicted one
-- makes its cursor `stale_cursor`. `snapshot_id` is never reused.
CREATE TABLE lane_list_snapshots (
    snapshot_id INTEGER PRIMARY KEY AUTOINCREMENT,
    filter_hash TEXT NOT NULL,
    lead_thread_id TEXT NOT NULL,
    as_of_revision INTEGER NOT NULL
);
CREATE INDEX idx_lane_list_snapshots_lead ON lane_list_snapshots(lead_thread_id, snapshot_id);
CREATE TABLE lane_list_snapshot_rows (
    snapshot_id INTEGER NOT NULL REFERENCES lane_list_snapshots(snapshot_id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    lane_id TEXT NOT NULL,
    PRIMARY KEY (snapshot_id, position)
) WITHOUT ROWID;
