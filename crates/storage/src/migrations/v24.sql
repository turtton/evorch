-- Threads and the runs that belong to them (ADR 0041 §A-1).
--
-- sidebar.json stays the source of thread definitions: the GUI mirrors it here
-- (source = 'host') and the runtime records the threads it starts or branches
-- (source = 'runtime'). Host values win column by column. Rows inferred below
-- from older run metadata are 'backfill'. project_root is the canonical project
-- directory and the key of project visibility; NULL means unknown, and such a
-- thread is never visible by project.
CREATE TABLE threads (
    thread_id TEXT PRIMARY KEY,
    project_id TEXT,
    project_root TEXT,
    title TEXT,
    root_run_id TEXT,
    chat_role TEXT,
    parent_thread_id TEXT,
    lineage_kind TEXT CHECK (lineage_kind IN ('fork', 'rewind', 'escalation')),
    fork_point_run_id TEXT,
    fork_point_context_len INTEGER CHECK (fork_point_context_len >= 0),
    escalation_source_run_id TEXT,
    archived INTEGER NOT NULL DEFAULT 0 CHECK (archived IN (0, 1)),
    superseded INTEGER NOT NULL DEFAULT 0 CHECK (superseded IN (0, 1)),
    created_at_ns INTEGER NOT NULL,
    updated_at_ns INTEGER NOT NULL,
    source TEXT NOT NULL CHECK (source IN ('runtime', 'host', 'backfill'))
);
CREATE INDEX idx_threads_project_updated ON threads(project_root, updated_at_ns DESC);
CREATE INDEX idx_threads_parent ON threads(parent_thread_id);

-- The thread a run belongs to. depth counts parent links from root_run_id, the
-- conversation root the run descends from.
CREATE TABLE thread_runs (
    run_id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL,
    root_run_id TEXT NOT NULL,
    depth INTEGER NOT NULL CHECK (depth >= 0)
);
CREATE INDEX idx_thread_runs_thread ON thread_runs(thread_id);

-- Delegation trees are walked downward through both kinds of parent link.
CREATE INDEX idx_run_contexts_parent ON run_contexts(parent_run_id);
CREATE INDEX idx_usage_requests_parent ON usage_requests(parent_run_id);

-- Backfill from run metadata only; message bodies are never read.
--
-- 1. Conversation roots named `chat:<Role>:<thread>`.
INSERT OR IGNORE INTO thread_runs (run_id, thread_id, root_run_id, depth)
SELECT run_id, substr(name, 6 + instr(substr(name, 6), ':')), run_id, 0
FROM run_contexts
WHERE parent_run_id IS NULL
    AND substr(name, 1, 5) = 'chat:'
    AND instr(substr(name, 6), ':') > 1
    AND length(substr(name, 6 + instr(substr(name, 6), ':'))) > 0;

-- 2. Roots recorded with a thread start state.
INSERT OR IGNORE INTO thread_runs (run_id, thread_id, root_run_id, depth)
SELECT root_run_id, thread_id, root_run_id, 0 FROM thread_start_states;

-- 3. Saved roots the GUI attributed to a thread.
INSERT OR IGNORE INTO thread_runs (run_id, thread_id, root_run_id, depth)
SELECT a.run_id, a.thread_id, a.run_id, 0
FROM usage_run_threads a JOIN run_contexts c ON c.run_id = a.run_id
WHERE c.parent_run_id IS NULL;

-- 4. Their delegated descendants, through saved contexts and the usage ledger.
INSERT OR IGNORE INTO thread_runs (run_id, thread_id, root_run_id, depth)
WITH RECURSIVE tree(run_id, thread_id, root_run_id, depth) AS (
    SELECT run_id, thread_id, root_run_id, depth FROM thread_runs
    UNION
    SELECT c.run_id, tree.thread_id, tree.root_run_id, tree.depth + 1
    FROM run_contexts c JOIN tree ON c.parent_run_id = tree.run_id
    WHERE tree.depth < 64
    UNION
    SELECT u.run_id, tree.thread_id, tree.root_run_id, tree.depth + 1
    FROM usage_requests u JOIN tree ON u.parent_run_id = tree.run_id
    WHERE u.run_id IS NOT NULL AND tree.depth < 64
)
SELECT run_id, thread_id, root_run_id, MIN(depth) FROM tree GROUP BY run_id;

-- 5. One thread per bound thread ID. The most recent root is the current root.
-- The project comes from the thread start, else from the runs' saved
-- configuration (roots first); a project that cannot be inferred stays NULL.
INSERT OR IGNORE INTO threads (
    thread_id, project_id, project_root, title, root_run_id, chat_role, parent_thread_id,
    lineage_kind, fork_point_run_id, fork_point_context_len, escalation_source_run_id,
    archived, superseded, created_at_ns, updated_at_ns, source
)
WITH activity AS (
    SELECT tr.thread_id, MIN(c.updated_at_ns) AS first_ns, MAX(c.updated_at_ns) AS last_ns
    FROM thread_runs tr LEFT JOIN run_contexts c ON c.run_id = tr.run_id
    GROUP BY tr.thread_id
),
roots AS (
    SELECT tr.thread_id, tr.run_id, c.name,
        ROW_NUMBER() OVER (
            PARTITION BY tr.thread_id
            ORDER BY COALESCE(c.updated_at_ns, 0) DESC, tr.run_id DESC
        ) AS recency
    FROM thread_runs tr LEFT JOIN run_contexts c ON c.run_id = tr.run_id
    WHERE tr.depth = 0
),
configured AS (
    SELECT thread_id, project_root,
        ROW_NUMBER() OVER (
            PARTITION BY thread_id ORDER BY depth, updated_at_ns DESC, run_id DESC
        ) AS preference
    FROM (
        SELECT tr.thread_id, tr.run_id, tr.depth, c.updated_at_ns,
            CASE WHEN json_valid(c.config_json) THEN
                CASE WHEN json_type(c.config_json, '$.project_root') = 'text'
                    THEN json_extract(c.config_json, '$.project_root') END
            END AS project_root
        FROM thread_runs tr JOIN run_contexts c ON c.run_id = tr.run_id
    )
    WHERE project_root IS NOT NULL AND project_root <> ''
),
projects AS (
    SELECT thread_id, project_id,
        ROW_NUMBER() OVER (PARTITION BY thread_id ORDER BY run_id DESC) AS preference
    FROM usage_run_threads
    WHERE project_id IS NOT NULL
)
SELECT
    a.thread_id,
    p.project_id,
    COALESCE(s.project_root, cf.project_root),
    NULL,
    r.run_id,
    CASE WHEN substr(r.name, 1, 5) = 'chat:' AND instr(substr(r.name, 6), ':') > 1
        THEN substr(r.name, 6, instr(substr(r.name, 6), ':') - 1) END,
    NULL, NULL, NULL, NULL, NULL,
    0, 0,
    COALESCE(min(s.captured_at_ns, a.first_ns), s.captured_at_ns, a.first_ns, 0),
    COALESCE(max(s.captured_at_ns, a.last_ns), s.captured_at_ns, a.last_ns, 0),
    'backfill'
FROM activity a
LEFT JOIN roots r ON r.thread_id = a.thread_id AND r.recency = 1
LEFT JOIN configured cf ON cf.thread_id = a.thread_id AND cf.preference = 1
LEFT JOIN projects p ON p.thread_id = a.thread_id AND p.preference = 1
LEFT JOIN thread_start_states s ON s.thread_id = a.thread_id;

-- 6. An escalated conversation is a child of the thread whose run requested it.
UPDATE threads SET
    lineage_kind = 'escalation',
    escalation_source_run_id = escalation.source_run_id,
    parent_thread_id = (
        SELECT source.thread_id FROM thread_runs source
        WHERE source.run_id = escalation.source_run_id
    )
FROM (
    SELECT source_run_id, new_run_id FROM (
        SELECT
            CASE WHEN json_valid(payload) THEN json_extract(payload, '$.payload.kind') END
                AS event,
            CASE WHEN json_valid(payload)
                THEN json_extract(payload, '$.payload.payload.source_run_id') END
                AS source_run_id,
            CASE WHEN json_valid(payload)
                THEN json_extract(payload, '$.payload.payload.new_run_id') END
                AS new_run_id
        FROM events
        WHERE kind = 'Lifecycle' AND instr(payload, '"EscalationRequested"') > 0
    )
    WHERE event = 'EscalationRequested'
        AND source_run_id IS NOT NULL
        AND new_run_id IS NOT NULL
) AS escalation
JOIN thread_runs escalated ON escalated.run_id = escalation.new_run_id AND escalated.depth = 0
WHERE threads.thread_id = escalated.thread_id AND threads.source = 'backfill';
