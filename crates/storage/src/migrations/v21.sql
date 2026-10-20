-- The branch HEAD named when a thread started, so a replay checks out the same
-- branch (ADR 0036 §3). NULL when HEAD was detached, and for states recorded
-- before this column existed.
ALTER TABLE thread_start_states ADD COLUMN head_ref TEXT;
