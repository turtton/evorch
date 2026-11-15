-- Restart membership is evidence, never authority reconstructed at cold boot.
CREATE TABLE restart_learning_inventory (
 restart_id TEXT PRIMARY KEY REFERENCES restart_transactions(restart_id),
 inventory_json TEXT NOT NULL
);
CREATE TABLE restart_learning_members (
 restart_id TEXT NOT NULL REFERENCES restart_transactions(restart_id),
 task_id TEXT NOT NULL, member_json TEXT NOT NULL,
 disposition TEXT NOT NULL DEFAULT 'pending', result_key_json TEXT,
 PRIMARY KEY(restart_id,task_id)
);
CREATE TABLE restart_run_tools (
 restart_id TEXT NOT NULL REFERENCES restart_transactions(restart_id),
 run_id TEXT NOT NULL, incarnation TEXT NOT NULL, old_snapshot_json TEXT NOT NULL, evidence_json TEXT,
 consumed INTEGER NOT NULL DEFAULT 0 CHECK(consumed IN (0,1)),
 PRIMARY KEY(restart_id,run_id,incarnation)
);
ALTER TABLE restart_transactions ADD COLUMN boot_progress_json TEXT;
