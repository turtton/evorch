-- Archive decisions read their sources directly; nothing derived is stored.
-- Indexes only: no table, trigger or backfill, so the migration scans nothing.
CREATE INDEX idx_retention_questions_root_pending ON user_questions(root_run_id,answered);
-- Newest goal snapshot of a run is an index seek; the partial index holds only goal rows.
CREATE INDEX idx_events_goal_by_run ON events(run_id, id) WHERE kind = 'Orchestrator' AND subkind = 'ThreadGoalUpdated' AND run_id IS NOT NULL;
CREATE INDEX host_input_receipts_run_accepted ON host_input_receipts(run_id, accepted_at_ns);
CREATE INDEX host_chat_submissions_run_accepted ON host_chat_submissions(run_id, accepted_at_ns);
