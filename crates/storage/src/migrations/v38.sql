-- The host numbers every archive and pin edit (PR-4), so the "last writer sent no
-- revision" markers of the archive and pin series are no longer needed.
ALTER TABLE threads DROP COLUMN archive_unnumbered;
ALTER TABLE threads DROP COLUMN pin_unnumbered;
