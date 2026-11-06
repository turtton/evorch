-- The title of a pull request, for display only. It is adopted with the facts of the same
-- fetch (the same success_seq) but is not one of them: no review, manifest or agent input
-- reads it. NULL when no fetch gave one that could be kept.
ALTER TABLE pull_request_snapshots ADD COLUMN title TEXT;
