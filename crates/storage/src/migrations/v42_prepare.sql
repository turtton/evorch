-- Message-level provenance storage (PR A2c): everything of migration v42 except the
-- removal of the old column (`v42_drop.sql`).
--
-- `run_context_provenance` keeps the status, the evidence and a small header of the
-- map; the map's messages move to `run_message_provenance`, one row per
-- `ProvenanceMap.messages` entry, so a save writes only the entries that changed.
--
-- This migration is the shape of a NEW database. A database that already holds maps in
-- the old column is never migrated here: `Database::open` refuses it
-- (`StorageError::ConversionRequired`) and the conversion command rewrites it.

-- The header of the map. `map_message_count` is the number of child rows; the others are
-- the map's own scalar fields.
ALTER TABLE run_context_provenance ADD COLUMN map_version INTEGER NOT NULL DEFAULT 0 CHECK (map_version >= 0);
ALTER TABLE run_context_provenance ADD COLUMN raw_message_count INTEGER NOT NULL DEFAULT 0 CHECK (raw_message_count >= 0);
ALTER TABLE run_context_provenance ADD COLUMN checkpoint_count INTEGER NOT NULL DEFAULT 0 CHECK (checkpoint_count >= 0);
ALTER TABLE run_context_provenance ADD COLUMN map_message_count INTEGER NOT NULL DEFAULT 0 CHECK (map_message_count >= 0);

-- One `MessageProvenance` per row. `map_index` is the entry's position in
-- `ProvenanceMap.messages` (not its `raw_index` or `ordinal`, which stay inside
-- `entry_json`: a map may be incomplete or repeat an index). `entry_sha256` and
-- `entry_len` describe `entry_json`, so a save compares them without reading it;
-- `entry_json` is last for the same reason. It is provenance metadata, never a body.
-- Children are deleted before their header (RESTRICT, not CASCADE): retention reclaims
-- them in bounded steps.
CREATE TABLE run_message_provenance (
    id           INTEGER PRIMARY KEY,
    run_id       TEXT NOT NULL REFERENCES run_context_provenance(run_id) ON DELETE RESTRICT,
    map_index    INTEGER NOT NULL CHECK (map_index >= 0),
    entry_sha256 BLOB NOT NULL CHECK (length(entry_sha256) = 32),
    entry_len    INTEGER NOT NULL CHECK (entry_len >= 0),
    entry_json   TEXT NOT NULL,
    UNIQUE (run_id, map_index)
);

-- Nothing may be stored for a tombstoned run, and a row of one may not move to another.
CREATE TRIGGER fence_run_message_provenance_insert BEFORE INSERT ON run_message_provenance
WHEN NEW.run_id IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
CREATE TRIGGER fence_run_message_provenance_update BEFORE UPDATE ON run_message_provenance
WHEN NEW.run_id IN (SELECT run_id FROM run_tombstones) OR OLD.run_id IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
