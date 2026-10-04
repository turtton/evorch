-- Per-request usage ledger. One row per provider attempt terminal event
-- (RequestCompleted / RequestFailed). cost_usd is the price at record time;
-- NULL means the model had no known price when the row was written.
CREATE TABLE usage_requests (
    request_id         TEXT PRIMARY KEY,
    at_ns              INTEGER NOT NULL,
    provider           TEXT NOT NULL,
    profile            TEXT,
    model              TEXT NOT NULL,
    run_id             TEXT,
    parent_run_id      TEXT,
    role               TEXT,
    purpose            TEXT,
    status             TEXT NOT NULL CHECK (status IN ('ok', 'failed')),
    failure            TEXT,
    finish_reason      TEXT,
    input_tokens       INTEGER NOT NULL DEFAULT 0,
    output_tokens      INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens  INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
    reasoning_tokens   INTEGER,
    ttft_ms            INTEGER,
    duration_ms        INTEGER NOT NULL DEFAULT 0,
    cost_usd           REAL
);
CREATE INDEX idx_usage_requests_at ON usage_requests(at_ns);
CREATE INDEX idx_usage_requests_run ON usage_requests(run_id);

-- Thread/project ownership is a GUI concept that can be bound after the
-- request is recorded, so it lives beside the ledger and is joined on read.
CREATE TABLE usage_run_threads (
    run_id     TEXT PRIMARY KEY,
    thread_id  TEXT NOT NULL,
    project_id TEXT
) WITHOUT ROWID;

-- Daily totals of requests older than the retention window. Empty strings
-- stand for unknown dimensions so they can participate in the primary key.
-- unpriced_* keep the tokens of rows without a recorded cost, so a viewer can
-- still price them with current rates.
CREATE TABLE usage_daily (
    day                         TEXT NOT NULL,
    provider                    TEXT NOT NULL,
    profile                     TEXT NOT NULL,
    model                       TEXT NOT NULL,
    project_id                  TEXT NOT NULL,
    role                        TEXT NOT NULL,
    purpose                     TEXT NOT NULL,
    request_count               INTEGER NOT NULL DEFAULT 0,
    failed_count                INTEGER NOT NULL DEFAULT 0,
    input_tokens                INTEGER NOT NULL DEFAULT 0,
    output_tokens               INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens           INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens          INTEGER NOT NULL DEFAULT 0,
    reasoning_tokens            INTEGER NOT NULL DEFAULT 0,
    cost_usd                    REAL NOT NULL DEFAULT 0,
    unpriced_input_tokens       INTEGER NOT NULL DEFAULT 0,
    unpriced_output_tokens      INTEGER NOT NULL DEFAULT 0,
    unpriced_cache_read_tokens  INTEGER NOT NULL DEFAULT 0,
    unpriced_cache_write_tokens INTEGER NOT NULL DEFAULT 0,
    ttft_sum_ms                 INTEGER NOT NULL DEFAULT 0,
    ttft_count                  INTEGER NOT NULL DEFAULT 0,
    duration_sum_ms             INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (day, provider, profile, model, project_id, role, purpose)
) WITHOUT ROWID;

-- Backfill once from persisted provider events. Legacy rows have no recorded
-- price, reasoning breakdown or purpose; purpose is inferred from the
-- auxiliary run IDs that existed before it was recorded.
INSERT OR IGNORE INTO usage_requests (
    request_id, at_ns, provider, profile, model, run_id, purpose, status, failure,
    finish_reason, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
    duration_ms
)
SELECT
    json_extract(payload, '$.payload.payload.request_id'),
    wall_clock_ns,
    json_extract(payload, '$.payload.payload.provider'),
    json_extract(payload, '$.payload.payload.profile'),
    json_extract(payload, '$.payload.payload.model'),
    json_extract(payload, '$.payload.payload.run_id'),
    CASE
        WHEN json_extract(payload, '$.payload.payload.run_id') IS NULL THEN NULL
        WHEN json_extract(payload, '$.payload.payload.run_id') = 'entry-routing' THEN 'routing'
        WHEN json_extract(payload, '$.payload.payload.run_id') LIKE 'auto-title:%' THEN 'title'
        ELSE 'agent'
    END,
    CASE json_extract(payload, '$.payload.kind') WHEN 'RequestCompleted' THEN 'ok' ELSE 'failed' END,
    CASE json_extract(payload, '$.payload.payload.failure.kind')
        WHEN 'Http' THEN 'Http:' || json_extract(payload, '$.payload.payload.failure.status')
        ELSE json_extract(payload, '$.payload.payload.failure.kind')
    END,
    json_extract(payload, '$.payload.payload.finish_reason'),
    COALESCE(json_extract(payload, '$.payload.payload.input_tokens'), 0),
    COALESCE(json_extract(payload, '$.payload.payload.output_tokens'), 0),
    COALESCE(json_extract(payload, '$.payload.payload.cache_read_tokens'), 0),
    COALESCE(json_extract(payload, '$.payload.payload.cache_write_tokens'), 0),
    COALESCE(json_extract(payload, '$.payload.payload.duration_ms'), 0)
FROM events
WHERE kind = 'Provider'
    AND json_valid(payload)
    AND json_extract(payload, '$.payload.kind') IN ('RequestCompleted', 'RequestFailed')
    AND json_extract(payload, '$.payload.payload.request_id') IS NOT NULL
ORDER BY id;

UPDATE usage_requests SET ttft_ms = first_token.ttft_ms
FROM (
    SELECT json_extract(payload, '$.payload.payload.request_id') AS request_id,
           MIN(json_extract(payload, '$.payload.payload.ttft_ms')) AS ttft_ms
    FROM events
    WHERE kind = 'Provider'
        AND json_valid(payload)
        AND json_extract(payload, '$.payload.kind') = 'FirstTokenObserved'
    GROUP BY 1
) AS first_token
WHERE usage_requests.request_id = first_token.request_id;

UPDATE usage_requests SET role = started.role, parent_run_id = started.parent_run_id
FROM (
    SELECT json_extract(payload, '$.payload.payload.run_id') AS run_id,
           json_extract(payload, '$.payload.payload.role') AS role,
           json_extract(payload, '$.payload.payload.parent_run_id') AS parent_run_id
    FROM events
    WHERE kind = 'Lifecycle'
        AND json_valid(payload)
        AND json_extract(payload, '$.payload.kind') = 'AgentRunStarted'
    GROUP BY 1
) AS started
WHERE usage_requests.run_id = started.run_id;
