pub(super) const V1: &str = r#"
CREATE TABLE sessions (
    id                TEXT PRIMARY KEY,
    parent_id         TEXT REFERENCES sessions(id),
    status            TEXT NOT NULL CHECK (status IN ('running','delegated','completed','failed')),
    failure_reason    TEXT,
    delegated_to      TEXT,
    total_event_bytes INTEGER NOT NULL DEFAULT 0,
    created_at_ns     INTEGER NOT NULL,
    updated_at_ns     INTEGER NOT NULL
);
CREATE INDEX idx_sessions_status ON sessions(status);
CREATE TABLE tasks (
    id            TEXT PRIMARY KEY,
    session_id    TEXT REFERENCES sessions(id),
    status        TEXT NOT NULL CHECK (status IN ('running','completed','failed')),
    created_at_ns INTEGER NOT NULL,
    updated_at_ns INTEGER NOT NULL
);
CREATE INDEX idx_tasks_session_id ON tasks(session_id);
CREATE TABLE messages (
    id            TEXT PRIMARY KEY,
    session_id    TEXT NOT NULL REFERENCES sessions(id),
    role          TEXT NOT NULL CHECK (role IN ('user','assistant','system','tool')),
    content       TEXT NOT NULL,
    reasoning     TEXT,
    created_at_ns INTEGER NOT NULL,
    updated_at_ns INTEGER NOT NULL
);
CREATE INDEX idx_messages_session_created ON messages(session_id, created_at_ns);
CREATE TABLE agent_runs (
    id             TEXT PRIMARY KEY,
    session_id     TEXT NOT NULL REFERENCES sessions(id),
    provider       TEXT NOT NULL,
    model          TEXT NOT NULL,
    status         TEXT NOT NULL CHECK (status IN ('running','completed','failed')),
    started_at_ns  INTEGER NOT NULL,
    finished_at_ns INTEGER
);
CREATE INDEX idx_agent_runs_session_id ON agent_runs(session_id);
CREATE TABLE events (
    id             INTEGER PRIMARY KEY,
    session_id     TEXT,
    schema_version INTEGER NOT NULL,
    monotonic_ns   INTEGER NOT NULL,
    wall_clock_ns  INTEGER NOT NULL,
    kind           TEXT NOT NULL,
    payload        TEXT NOT NULL
);
CREATE INDEX idx_events_session_id ON events(session_id, id);
CREATE INDEX idx_events_wall_clock ON events(wall_clock_ns);
CREATE TABLE downsampled_metrics (
    window_start       INTEGER NOT NULL,
    provider           TEXT NOT NULL,
    model              TEXT NOT NULL,
    input_tokens       INTEGER NOT NULL DEFAULT 0,
    output_tokens      INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens  INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
    cache_hits         INTEGER NOT NULL DEFAULT 0,
    cache_misses       INTEGER NOT NULL DEFAULT 0,
    request_count      INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (window_start, provider, model)
);
"#;

pub(super) const V2: &str = r#"
CREATE TABLE catalog_updates (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    source         TEXT NOT NULL,
    model_count    INTEGER NOT NULL,
    detail         TEXT NOT NULL,
    recorded_at_ns INTEGER NOT NULL
);
"#;
