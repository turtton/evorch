-- A question can be cancelled without an answer by the offline archive-blocker cleanup.
-- `cancelled` is a terminal state beside `answered`; the two never hold together, and a
-- cancelled question keeps no answer and no answer origin. Pending is answered=0 AND cancelled=0.
ALTER TABLE user_questions ADD COLUMN cancelled INTEGER NOT NULL DEFAULT 0 CHECK(cancelled IN (0,1));
-- The payload carries the same flag (absent means not cancelled); the column is what queries read.
CREATE TRIGGER user_questions_cancelled_insert BEFORE INSERT ON user_questions
WHEN json_valid(NEW.payload) AND (NEW.cancelled <> COALESCE(json_extract(NEW.payload,'$.cancelled'),0)
 OR (NEW.cancelled = 1 AND (NEW.answered <> 0 OR json_extract(NEW.payload,'$.answer') IS NOT NULL OR json_extract(NEW.payload,'$.answer_origin') IS NOT NULL)))
BEGIN SELECT RAISE(ABORT,'cancelled question state is inconsistent'); END;
CREATE TRIGGER user_questions_cancelled_update BEFORE UPDATE ON user_questions
WHEN (json_valid(NEW.payload) AND NEW.cancelled <> COALESCE(json_extract(NEW.payload,'$.cancelled'),0))
 OR (NEW.cancelled = 1 AND (NEW.answered <> 0 OR (json_valid(NEW.payload) AND (json_extract(NEW.payload,'$.answer') IS NOT NULL OR json_extract(NEW.payload,'$.answer_origin') IS NOT NULL))))
 OR (OLD.cancelled = 1 AND NEW.cancelled = 0)
 OR (OLD.cancelled = 0 AND NEW.cancelled = 1 AND OLD.answered <> 0)
BEGIN SELECT RAISE(ABORT,'cancelled question state is inconsistent or terminal'); END;
-- Evidence that the one-time archive-blocker cleanup completed. Counts and hashes only:
-- no question text, answer or target list is kept here.
CREATE TABLE archive_blocker_cleanup_receipts (
 plan_hash TEXT PRIMARY KEY CHECK(length(plan_hash) = 64),
 manifest_sha256 TEXT NOT NULL CHECK(length(manifest_sha256) = 64),
 source_sha256 TEXT NOT NULL CHECK(length(source_sha256) = 64),
 backup_sha256 TEXT NOT NULL CHECK(length(backup_sha256) = 64),
 cancelled_questions INTEGER NOT NULL CHECK(cancelled_questions >= 0),
 obsoleted_notifications INTEGER NOT NULL CHECK(obsoleted_notifications >= 0),
 closed_lanes INTEGER NOT NULL CHECK(closed_lanes >= 0),
 completed_at_ns INTEGER NOT NULL
);
CREATE TRIGGER archive_blocker_cleanup_receipts_update BEFORE UPDATE ON archive_blocker_cleanup_receipts
BEGIN SELECT RAISE(ABORT,'cleanup receipt is immutable'); END;
CREATE TRIGGER archive_blocker_cleanup_receipts_delete BEFORE DELETE ON archive_blocker_cleanup_receipts
BEGIN SELECT RAISE(ABORT,'cleanup receipt is immutable'); END;
