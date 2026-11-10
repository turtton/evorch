-- The window that holds the destination thread tells the person once per exhausted budget
-- that auto fix stopped. The mark is shared by every window and survives a restart.
ALTER TABLE pr_auto_fix_limit_events ADD COLUMN notified_at_ns INTEGER;
-- Stops recorded before this column existed are not told again.
UPDATE pr_auto_fix_limit_events SET notified_at_ns = at_ns;
