-- Reverts the pull request tables (migration 46). Apply it before undo_v45.sql.
DROP TABLE thread_pull_request_inheritance;
DROP TABLE thread_pull_request_candidates;
DROP TABLE pull_request_snapshots;
DROP TABLE thread_pull_requests;
DROP TABLE thread_pull_request_state;
