-- Threads pinned as evaluation samples (ADR 0036 §1). A sample needs a
-- replayable start state, and pins it: the start state is not deleted while
-- the sample exists.
CREATE TABLE eval_samples (
    thread_id TEXT PRIMARY KEY REFERENCES thread_start_states(thread_id),
    problem TEXT NOT NULL CHECK (problem <> ''),
    -- JSON array of role config addresses.
    focus_roles TEXT NOT NULL CHECK (json_valid(focus_roles) AND json_type(focus_roles) = 'array'),
    created_at_ns INTEGER NOT NULL,
    updated_at_ns INTEGER NOT NULL
);
CREATE TRIGGER eval_samples_replayable
BEFORE INSERT ON eval_samples
WHEN (SELECT replayable FROM thread_start_states WHERE thread_id = NEW.thread_id) IS NOT 1
BEGIN
    SELECT RAISE(ABORT, 'an evaluation sample requires a replayable thread start state');
END;
