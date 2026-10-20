-- Working-tree state recorded before the first root run of a new thread, so the
-- thread can later be replayed from the same start (ADR 0036 §2). A row that
-- cannot be replayed keeps only its reason, never partial content.
CREATE TABLE thread_start_states (
    thread_id TEXT PRIMARY KEY,
    root_run_id TEXT NOT NULL,
    project_root TEXT NOT NULL,
    repo_prefix TEXT,
    head_commit TEXT,
    diff BLOB,
    captured_at_ns INTEGER NOT NULL,
    replayable INTEGER NOT NULL CHECK (replayable IN (0, 1)),
    reason TEXT CHECK (reason IN (
        'not_git_repository', 'no_commit', 'too_large',
        'contains_secret', 'timed_out', 'capture_failed'
    )),
    CHECK (
        (replayable = 1 AND reason IS NULL AND repo_prefix IS NOT NULL
            AND head_commit IS NOT NULL AND diff IS NOT NULL)
        OR (replayable = 0 AND reason IS NOT NULL AND repo_prefix IS NULL
            AND head_commit IS NULL AND diff IS NULL)
    )
);
CREATE INDEX idx_thread_start_states_captured ON thread_start_states(captured_at_ns);
CREATE TABLE thread_start_untracked (
    thread_id TEXT NOT NULL REFERENCES thread_start_states(thread_id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('file', 'symlink')),
    executable INTEGER NOT NULL CHECK (executable IN (0, 1)),
    content BLOB NOT NULL,
    PRIMARY KEY (thread_id, path)
);
CREATE TRIGGER thread_start_untracked_replayable
BEFORE INSERT ON thread_start_untracked
WHEN (SELECT replayable FROM thread_start_states WHERE thread_id = NEW.thread_id) IS NOT 1
BEGIN
    SELECT RAISE(ABORT, 'untracked content requires a replayable thread start state');
END;
