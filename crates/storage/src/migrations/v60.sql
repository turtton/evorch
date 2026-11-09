-- Active executions and safe-point receipts. Payload is owned by runtime, not authority.
CREATE TABLE resume_intents (
    run_id TEXT PRIMARY KEY NOT NULL,
    incarnation TEXT NOT NULL,
    payload TEXT NOT NULL
);
CREATE TRIGGER fence_resume_intents_insert BEFORE INSERT ON resume_intents
WHEN EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=NEW.run_id)
BEGIN SELECT RAISE(ABORT,'run is tombstoned'); END;
CREATE TRIGGER fence_resume_intents_update BEFORE UPDATE ON resume_intents
WHEN EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=NEW.run_id)
BEGIN SELECT RAISE(ABORT,'run is tombstoned'); END;
