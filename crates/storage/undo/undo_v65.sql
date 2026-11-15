-- Diagnostic and handoff evidence is removed only by explicit offline downgrade fixtures.
DROP TABLE restart_run_tools;
DROP TABLE restart_learning_members;
DROP TABLE restart_learning_inventory;
ALTER TABLE restart_transactions DROP COLUMN boot_progress_json;
