DROP INDEX IF EXISTS idx_retention_questions_root_pending;
DROP INDEX IF EXISTS idx_events_goal_by_run;
DROP INDEX IF EXISTS host_input_receipts_run_accepted;
DROP INDEX IF EXISTS host_chat_submissions_run_accepted;
PRAGMA user_version = 74;
