-- The host numbers every archive and pin edit (PR-4), so the "last writer sent no
-- revision" markers of the archive and pin series are no longer needed.
ALTER TABLE threads DROP COLUMN archive_unnumbered;
ALTER TABLE threads DROP COLUMN pin_unnumbered;

-- Why an exploration could not resolve a family. The staging rows of such a family are
-- dropped (nothing can be confirmed), so the schedule would otherwise forget the
-- reason and call the thread "due". Rewritten by every exploration.
CREATE TABLE retention_unresolved (
    thread_id TEXT PRIMARY KEY,
    reason_id TEXT NOT NULL
) WITHOUT ROWID;
