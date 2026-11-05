-- A chat message a host accepted, with a fingerprint of the whole message, so a resend
-- under the same submission id can be told from a different message after a restart.
-- host_input_receipts keeps only the words; this keeps the identity. A row exists only
-- for messages accepted after this migration; an id with a receipt but no row here
-- cannot be compared and is refused.
CREATE TABLE host_chat_submissions (
    submission_id TEXT PRIMARY KEY,
    fingerprint TEXT NOT NULL,
    run_id TEXT NOT NULL,
    provenance_json TEXT NOT NULL,
    accepted_at_ns INTEGER NOT NULL
);
CREATE INDEX host_chat_submissions_run ON host_chat_submissions(run_id);
CREATE TRIGGER fence_host_chat_submissions_insert BEFORE INSERT ON host_chat_submissions
WHEN NEW.run_id IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
