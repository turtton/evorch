-- The conversation procedure (todo) list is retired: a handoff no longer carries it.
ALTER TABLE lane_handoffs DROP COLUMN todo_json;
