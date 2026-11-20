-- Durable administrative audit (5a-I): irreversible operations and aggregated denials.
--
-- accepted_admin_operations: one row per administrative operation ID. The accepted
-- facts are immutable; the row only moves forward prepared -> started -> finished.
-- Unfinished rows are never deleted. Every row is accounted with budget_bytes, the
-- serialized record plus a reserve for the fields a finished row gains later, so no
-- state change can grow a row past the 4 KiB record limit. The row and byte caps
-- (10,000 rows / 32 MiB) and the 30 day retention of finished rows are enforced by
-- the writer; CHECKs and triggers below only guard the shape and the direction.
CREATE TABLE accepted_admin_operations (
    operation_id TEXT PRIMARY KEY NOT NULL
        CHECK (length(CAST(operation_id AS BLOB)) BETWEEN 1 AND 128),
    attempt_nonce TEXT NOT NULL CHECK (length(attempt_nonce) = 36),
    accepted_core_epoch INTEGER NOT NULL CHECK (accepted_core_epoch >= 0),
    operation TEXT NOT NULL CHECK (operation IN (
        'restart_request', 'cancel', 'interrupt', 'discard', 'release', 'resume')),
    accepted_at_ms INTEGER NOT NULL CHECK (accepted_at_ms >= 0),
    accepted_json TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('prepared', 'started', 'finished')),
    restart_id TEXT
        CHECK (restart_id IS NULL OR length(CAST(restart_id AS BLOB)) BETWEEN 1 AND 128),
    outcome TEXT CHECK (outcome IS NULL OR outcome IN (
        'succeeded', 'failed', 'cancelled', 'conflict', 'unknown', 'aborted_before_effect')),
    finished_at_ms INTEGER CHECK (finished_at_ms IS NULL OR finished_at_ms >= 0),
    budget_bytes INTEGER NOT NULL
        CHECK (budget_bytes > 0 AND budget_bytes <= 4096),
    CHECK (budget_bytes >= length(CAST(accepted_json AS BLOB))),
    CHECK ((state = 'finished') = (outcome IS NOT NULL)),
    CHECK ((outcome IS NULL) = (finished_at_ms IS NULL)),
    CHECK (restart_id IS NULL OR operation = 'restart_request')
) WITHOUT ROWID;
CREATE INDEX idx_admin_operations_restart
    ON accepted_admin_operations(restart_id) WHERE restart_id IS NOT NULL;
CREATE INDEX idx_admin_operations_finished
    ON accepted_admin_operations(finished_at_ms, operation_id) WHERE state = 'finished';
CREATE INDEX idx_admin_operations_pending
    ON accepted_admin_operations(accepted_at_ms, operation_id) WHERE state != 'finished';

CREATE TRIGGER admin_operations_keep_unfinished
BEFORE DELETE ON accepted_admin_operations
WHEN OLD.state != 'finished'
BEGIN
    SELECT RAISE(ABORT, 'an unfinished administrative operation is kept');
END;

CREATE TRIGGER admin_operations_one_way
BEFORE UPDATE ON accepted_admin_operations
WHEN NEW.operation_id IS NOT OLD.operation_id
    OR NEW.attempt_nonce IS NOT OLD.attempt_nonce
    OR NEW.accepted_core_epoch IS NOT OLD.accepted_core_epoch
    OR NEW.operation IS NOT OLD.operation
    OR NEW.accepted_at_ms IS NOT OLD.accepted_at_ms
    OR NEW.accepted_json IS NOT OLD.accepted_json
    OR NEW.budget_bytes IS NOT OLD.budget_bytes
    OR (OLD.restart_id IS NOT NULL AND NEW.restart_id IS NOT OLD.restart_id)
    OR (OLD.state = 'started' AND NEW.state = 'prepared')
    OR (OLD.state = 'finished' AND (
        NEW.state IS NOT 'finished'
        OR NEW.outcome IS NOT OLD.outcome
        OR NEW.finished_at_ms IS NOT OLD.finished_at_ms
        OR NEW.restart_id IS NOT OLD.restart_id))
BEGIN
    SELECT RAISE(ABORT, 'an administrative operation only moves forward');
END;

-- admin_denials: refusals aggregated per 60 second window and bounded key. The key
-- never contains a client, session or request ID; keys beyond the per-window cap are
-- merged into the fixed overflow key. count is clamped to the SQLite signed range;
-- record_json keeps the exact u64. Capacity (1,000 rows / 4 MiB) and the 24 hour
-- retention are enforced by the writer and never take accepted_admin_operations room.
CREATE TABLE admin_denials (
    id INTEGER PRIMARY KEY,
    window_start_ms INTEGER NOT NULL
        CHECK (window_start_ms >= 0 AND window_start_ms % 60000 = 0),
    key_text TEXT NOT NULL CHECK (length(CAST(key_text AS BLOB)) BETWEEN 1 AND 1024),
    first_ms INTEGER NOT NULL,
    last_ms INTEGER NOT NULL,
    count INTEGER NOT NULL CHECK (count >= 1),
    record_json TEXT NOT NULL,
    budget_bytes INTEGER NOT NULL CHECK (budget_bytes > 0 AND budget_bytes <= 4096),
    CHECK (first_ms >= window_start_ms),
    CHECK (last_ms >= first_ms),
    CHECK (last_ms < window_start_ms + 60000),
    CHECK (budget_bytes >= length(CAST(record_json AS BLOB))),
    UNIQUE (window_start_ms, key_text)
);
CREATE INDEX idx_admin_denials_window ON admin_denials(window_start_ms, id);
