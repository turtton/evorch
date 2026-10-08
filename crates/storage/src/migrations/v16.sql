-- Duplicates inside the cooldown no longer vanish: they bump the existing
-- candidate's occurrence count, last-seen time and bounded recent run list.
ALTER TABLE improvement_candidates ADD COLUMN occurrences INTEGER NOT NULL DEFAULT 1;
ALTER TABLE improvement_candidates ADD COLUMN last_seen_at_ns INTEGER NOT NULL DEFAULT 0;
ALTER TABLE improvement_candidates ADD COLUMN recent_run_ids TEXT NOT NULL DEFAULT '[]';
UPDATE improvement_candidates SET last_seen_at_ns = created_at_ns;
UPDATE improvement_candidates SET recent_run_ids = json_array(run_id) WHERE run_id IS NOT NULL;
