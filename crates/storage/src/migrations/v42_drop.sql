-- Message-level provenance storage (PR A2c), the step that removes the old column.
--
-- A new database runs this and then `v42_prepare.sql` (see MIGRATIONS). The conversion
-- command runs `v42_prepare.sql` first, converts every map, and runs this statement
-- last, once nothing reads the column any more.

-- The old whole-map column. No reader or writer of this build names it.
ALTER TABLE run_context_provenance DROP COLUMN map_json;
