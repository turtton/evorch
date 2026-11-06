-- Test-only reconstruction of v49; never a production downgrade.
ALTER TABLE pull_request_snapshots DROP COLUMN title;
