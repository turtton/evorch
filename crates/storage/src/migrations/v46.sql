-- Pull request integration (ADR 0043 Phase 1, persistent contract).
--
-- A thread may be linked to one GitHub pull request at a time. The link, the
-- candidates a tool output suggested, the inheritance of a link by a derived thread and
-- the facts fetched about the pull request are kept here. The facts are structured
-- fields only: no pull request body, comment or log text is ever stored.
--
-- The pull request key is (provider, host, repository, number). `repository` is the
-- BASE repository ("owner/name", lowercase); the head repository is a separate field of
-- the snapshot. Phase 1 knows GitHub on github.com only.

-- 1. Per-thread lifecycle state. It outlives every link: `lifecycle_revision` counts the
-- link / unlink decisions of the thread, so a stale window that read revision N cannot
-- unlink (or replace) what a later decision linked. `transition_seq` numbers the state
-- transitions the runtime derives from snapshots; `notified_json` keeps the notification
-- keys that were already sent (bounded, see `notified_floor_seq`); the local HEAD
-- observation is bound to the link revision it was made for.
CREATE TABLE thread_pull_request_state (
    thread_id TEXT PRIMARY KEY,
    lifecycle_revision INTEGER NOT NULL DEFAULT 0 CHECK (lifecycle_revision >= 0),
    transition_seq INTEGER NOT NULL DEFAULT 0 CHECK (transition_seq >= 0),
    notified_json TEXT NOT NULL DEFAULT '[]',
    notified_floor_seq INTEGER NOT NULL DEFAULT 0 CHECK (notified_floor_seq >= 0),
    local_head_link_revision INTEGER CHECK (local_head_link_revision >= 1),
    local_head_sha TEXT,
    local_head_observed_at_ns INTEGER,
    CHECK ((local_head_link_revision IS NULL) = (local_head_sha IS NULL)),
    CHECK ((local_head_sha IS NULL) = (local_head_observed_at_ns IS NULL))
) WITHOUT ROWID;
CREATE TRIGGER thread_pull_request_state_monotonic BEFORE UPDATE ON thread_pull_request_state
WHEN NEW.thread_id <> OLD.thread_id OR NEW.lifecycle_revision < OLD.lifecycle_revision
    OR NEW.transition_seq < OLD.transition_seq OR NEW.notified_floor_seq < OLD.notified_floor_seq
BEGIN SELECT RAISE(ABORT, 'pull request counters never go back'); END;
CREATE TRIGGER fence_thread_pull_request_state_insert BEFORE INSERT ON thread_pull_request_state
WHEN NEW.thread_id IN (SELECT thread_id FROM thread_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
CREATE TRIGGER fence_thread_pull_request_state_update BEFORE UPDATE ON thread_pull_request_state
WHEN NEW.thread_id IN (SELECT thread_id FROM thread_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;

-- 2. The links of a thread, kept as history. At most one is active (not ended) per thread.
-- `link_revision` is the lifecycle revision that created the link; `end_revision` the
-- one that ended it (a relink or an unlink). `parent_link_id` is set exactly for an
-- inherited link and names the link of the parent thread it was taken from.
CREATE TABLE thread_pull_requests (
    link_id INTEGER PRIMARY KEY AUTOINCREMENT,
    thread_id TEXT NOT NULL,
    provider TEXT NOT NULL CHECK (provider IN ('github')),
    host TEXT NOT NULL CHECK (host = 'github.com'),
    repository TEXT NOT NULL CHECK (instr(repository, '/') > 1),
    number INTEGER NOT NULL CHECK (number >= 1),
    origin TEXT NOT NULL CHECK (origin IN ('explicit', 'inherited')),
    parent_link_id INTEGER,
    created_at_ns INTEGER NOT NULL,
    link_revision INTEGER NOT NULL CHECK (link_revision >= 1),
    superseded_at_ns INTEGER,
    end_reason TEXT CHECK (end_reason IN ('relinked', 'unlinked')),
    end_revision INTEGER CHECK (end_revision > link_revision),
    CHECK ((superseded_at_ns IS NULL) = (end_reason IS NULL)),
    CHECK ((superseded_at_ns IS NULL) = (end_revision IS NULL)),
    CHECK ((origin = 'inherited') = (parent_link_id IS NOT NULL)),
    UNIQUE (thread_id, link_revision)
);
CREATE UNIQUE INDEX idx_thread_pull_requests_active
    ON thread_pull_requests(thread_id) WHERE superseded_at_ns IS NULL;
CREATE INDEX idx_thread_pull_requests_key
    ON thread_pull_requests(provider, host, repository, number);
-- A link keeps its identity; it can only be ended, once.
CREATE TRIGGER thread_pull_requests_immutable BEFORE UPDATE ON thread_pull_requests
WHEN OLD.superseded_at_ns IS NOT NULL
    OR NEW.link_id <> OLD.link_id OR NEW.thread_id <> OLD.thread_id
    OR NEW.provider <> OLD.provider OR NEW.host <> OLD.host
    OR NEW.repository <> OLD.repository OR NEW.number <> OLD.number
    OR NEW.origin <> OLD.origin OR NEW.parent_link_id IS NOT OLD.parent_link_id
    OR NEW.created_at_ns <> OLD.created_at_ns OR NEW.link_revision <> OLD.link_revision
BEGIN SELECT RAISE(ABORT, 'a pull request link keeps its identity and is ended once'); END;
CREATE TRIGGER fence_thread_pull_requests_insert BEFORE INSERT ON thread_pull_requests
WHEN NEW.thread_id IN (SELECT thread_id FROM thread_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
CREATE TRIGGER fence_thread_pull_requests_update BEFORE UPDATE ON thread_pull_requests
WHEN NEW.thread_id IN (SELECT thread_id FROM thread_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;

-- 3. Facts about a pull request, shared by every thread that tracks it. A row is created
-- by the first fetch of the key. `fetch_seq` is handed out when a fetch STARTS;
-- `success_seq` is the sequence of the newest committed success and `last_attempt_seq`
-- that of the newest recorded outcome (success or failure). A result older than what is
-- stored is refused, and a failure never erases the last success. Rows are never deleted
-- (retention only clears the facts of an unreferenced key), so a counter never restarts.
-- `checks_json` is a bounded array of check summaries (sanitized names, no log text);
-- `review_decision` is GitHub's structured decision only, never a count or a comment.
CREATE TABLE pull_request_snapshots (
    provider TEXT NOT NULL CHECK (provider IN ('github')),
    host TEXT NOT NULL CHECK (host = 'github.com'),
    repository TEXT NOT NULL CHECK (instr(repository, '/') > 1),
    number INTEGER NOT NULL CHECK (number >= 1),
    state TEXT CHECK (state IN ('open', 'closed', 'merged')),
    base_branch TEXT,
    head_repository TEXT,
    head_branch TEXT,
    head_sha TEXT,
    checks_json TEXT NOT NULL DEFAULT '[]',
    checks_complete INTEGER NOT NULL DEFAULT 0 CHECK (checks_complete IN (0, 1)),
    review_decision TEXT CHECK (review_decision IN
        ('approved', 'changes_requested', 'review_required', 'none')),
    fetched_at_ns INTEGER,
    success_seq INTEGER NOT NULL DEFAULT 0 CHECK (success_seq >= 0),
    fetch_seq INTEGER NOT NULL DEFAULT 0 CHECK (fetch_seq >= 0),
    last_attempt_seq INTEGER NOT NULL DEFAULT 0 CHECK (last_attempt_seq >= 0),
    last_attempt_at_ns INTEGER,
    last_error_kind TEXT CHECK (last_error_kind IN
        ('unauthenticated', 'not_found', 'rate_limited', 'network', 'timeout',
         'invalid_response', 'other')),
    last_error_message TEXT,
    consecutive_failures INTEGER NOT NULL DEFAULT 0 CHECK (consecutive_failures >= 0),
    PRIMARY KEY (provider, host, repository, number),
    CHECK (state IS NULL OR (base_branch IS NOT NULL AND head_branch IS NOT NULL
        AND head_sha IS NOT NULL AND review_decision IS NOT NULL
        AND fetched_at_ns IS NOT NULL)),
    CHECK (success_seq <= fetch_seq AND last_attempt_seq <= fetch_seq),
    CHECK ((last_error_kind IS NULL) = (last_error_message IS NULL))
) WITHOUT ROWID;
CREATE TRIGGER pull_request_snapshots_monotonic BEFORE UPDATE ON pull_request_snapshots
WHEN NEW.fetch_seq < OLD.fetch_seq OR NEW.success_seq < OLD.success_seq
    OR NEW.last_attempt_seq < OLD.last_attempt_seq
BEGIN SELECT RAISE(ABORT, 'pull request fetch sequences never go back'); END;

-- 4. Pull requests that a tool output suggested for a thread. A person confirms or
-- dismisses one; nothing links by itself. One row per (thread, pull request): seeing the
-- same pull request again never revives a dismissed or confirmed candidate.
CREATE TABLE thread_pull_request_candidates (
    candidate_id INTEGER PRIMARY KEY AUTOINCREMENT,
    thread_id TEXT NOT NULL,
    provider TEXT NOT NULL CHECK (provider IN ('github')),
    host TEXT NOT NULL CHECK (host = 'github.com'),
    repository TEXT NOT NULL CHECK (instr(repository, '/') > 1),
    number INTEGER NOT NULL CHECK (number >= 1),
    source_run_id TEXT NOT NULL,
    source_call_id TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('presented', 'dismissed', 'confirmed')),
    created_at_ns INTEGER NOT NULL,
    updated_at_ns INTEGER NOT NULL,
    UNIQUE (thread_id, provider, host, repository, number)
);
CREATE TRIGGER thread_pull_request_candidates_immutable BEFORE UPDATE ON thread_pull_request_candidates
WHEN NEW.candidate_id <> OLD.candidate_id OR NEW.thread_id <> OLD.thread_id
    OR NEW.provider <> OLD.provider OR NEW.host <> OLD.host
    OR NEW.repository <> OLD.repository OR NEW.number <> OLD.number
    OR NEW.source_run_id <> OLD.source_run_id OR NEW.source_call_id <> OLD.source_call_id
    OR NEW.created_at_ns <> OLD.created_at_ns
    OR (OLD.state = 'confirmed' AND NEW.state <> 'confirmed')
BEGIN SELECT RAISE(ABORT, 'a pull request candidate keeps its identity and stays confirmed'); END;
CREATE TRIGGER fence_thread_pull_request_candidates_insert BEFORE INSERT ON thread_pull_request_candidates
WHEN NEW.thread_id IN (SELECT thread_id FROM thread_tombstones)
    OR NEW.source_run_id IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
CREATE TRIGGER fence_thread_pull_request_candidates_update BEFORE UPDATE ON thread_pull_request_candidates
WHEN NEW.thread_id IN (SELECT thread_id FROM thread_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;

-- 5. A derived thread's pending right to take its parent's link. The parent link is
-- pinned together with its pull request key, so the row stays valid when the parent
-- unlinks or is deleted. It is applied only while the child's lifecycle revision is
-- still `expected_child_revision` (no decision of the child in between) and the pull
-- request is confirmed open. An explicit link or unlink of the child cancels it in the
-- same transaction; a decided row never becomes pending again.
CREATE TABLE thread_pull_request_inheritance (
    child_thread_id TEXT PRIMARY KEY,
    parent_thread_id TEXT NOT NULL,
    parent_link_id INTEGER NOT NULL,
    provider TEXT NOT NULL CHECK (provider IN ('github')),
    host TEXT NOT NULL CHECK (host = 'github.com'),
    repository TEXT NOT NULL CHECK (instr(repository, '/') > 1),
    number INTEGER NOT NULL CHECK (number >= 1),
    expected_child_revision INTEGER NOT NULL CHECK (expected_child_revision >= 0),
    state TEXT NOT NULL CHECK (state IN ('pending', 'applied', 'skipped', 'cancelled')),
    decision_reason TEXT,
    applied_link_id INTEGER,
    attempt_count INTEGER NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
    last_attempt_at_ns INTEGER,
    last_attempt_note TEXT,
    created_at_ns INTEGER NOT NULL,
    decided_at_ns INTEGER,
    CHECK ((state = 'pending') = (decided_at_ns IS NULL)),
    CHECK ((state = 'applied') = (applied_link_id IS NOT NULL))
) WITHOUT ROWID;
CREATE INDEX idx_thread_pull_request_inheritance_pending
    ON thread_pull_request_inheritance(created_at_ns, child_thread_id) WHERE state = 'pending';
CREATE TRIGGER thread_pull_request_inheritance_immutable BEFORE UPDATE ON thread_pull_request_inheritance
WHEN NEW.child_thread_id <> OLD.child_thread_id OR NEW.parent_thread_id <> OLD.parent_thread_id
    OR NEW.parent_link_id <> OLD.parent_link_id OR NEW.provider <> OLD.provider
    OR NEW.host <> OLD.host OR NEW.repository <> OLD.repository OR NEW.number <> OLD.number
    OR NEW.expected_child_revision <> OLD.expected_child_revision
    OR NEW.created_at_ns <> OLD.created_at_ns
    OR NEW.attempt_count < OLD.attempt_count
    OR (OLD.state <> 'pending' AND (NEW.state <> OLD.state
        OR NEW.decision_reason IS NOT OLD.decision_reason
        OR NEW.applied_link_id IS NOT OLD.applied_link_id))
BEGIN SELECT RAISE(ABORT, 'a decided pull request inheritance never changes'); END;
CREATE TRIGGER fence_thread_pull_request_inheritance_insert BEFORE INSERT ON thread_pull_request_inheritance
WHEN NEW.child_thread_id IN (SELECT thread_id FROM thread_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
CREATE TRIGGER fence_thread_pull_request_inheritance_update BEFORE UPDATE ON thread_pull_request_inheritance
WHEN NEW.child_thread_id IN (SELECT thread_id FROM thread_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
