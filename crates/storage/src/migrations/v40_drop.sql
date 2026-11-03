-- Message-level conversation storage (PR A2b), the step that removes the old column.
--
-- A new database runs this and then `v40_prepare.sql` (see MIGRATIONS). The conversion
-- command runs `v40_prepare.sql` first, converts every run's messages, and runs this
-- statement last, once nothing reads the column any more.

-- The old whole-conversation column. No reader or writer of this build names it.
ALTER TABLE run_contexts DROP COLUMN messages_json;
