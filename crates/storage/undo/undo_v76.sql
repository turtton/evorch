DROP TRIGGER IF EXISTS archive_blocker_cleanup_receipts_delete;
DROP TRIGGER IF EXISTS archive_blocker_cleanup_receipts_update;
DROP TABLE IF EXISTS archive_blocker_cleanup_receipts;
DROP TRIGGER IF EXISTS user_questions_cancelled_update;
DROP TRIGGER IF EXISTS user_questions_cancelled_insert;
ALTER TABLE user_questions DROP COLUMN cancelled;
PRAGMA user_version = 75;
