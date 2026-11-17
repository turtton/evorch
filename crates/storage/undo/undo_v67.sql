-- Explicit offline downgrade fixture: the retired handoff todo column returns empty.
ALTER TABLE lane_handoffs ADD COLUMN todo_json TEXT;
