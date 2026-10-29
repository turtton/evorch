-- Event attribution columns. `subkind` is the inner tag of the category and
-- `run_id` the owning run (`Event::primary_run_id`). Rows written before this
-- version keep NULL `subkind` until the event backfill fills them; readers fall
-- back to the payload for those rows.
ALTER TABLE events ADD COLUMN subkind TEXT;
ALTER TABLE events ADD COLUMN run_id TEXT;
CREATE INDEX idx_events_kind_subkind_id ON events(kind, subkind, id);
CREATE INDEX idx_events_run_id ON events(run_id, id) WHERE run_id IS NOT NULL;
-- Holds only rows the backfill has not reached, so "is any row left?" and each
-- backfill batch cost nothing once the history is converted.
CREATE INDEX idx_events_unbackfilled ON events(id) WHERE subkind IS NULL;
-- Every production read selects by kind, run or id; the only session_id filters
-- left are test and projection-rebuild paths, which scan.
DROP INDEX idx_events_session_id;
