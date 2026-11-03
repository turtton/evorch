-- Message-level conversation storage (PR A2b): everything of migration v40 except the
-- removal of the old column (`v40_drop.sql`).
--
-- `run_contexts` becomes the header of a run: its configuration, checkpoint list and the
-- projections readers need without reading a message. The messages move to `run_messages`,
-- one row per provider message, so a save writes only the messages that changed.
--
-- This migration is the shape of a NEW database. A database that already holds snapshots
-- in the old shape is never migrated here: `Database::open` refuses it
-- (`StorageError::ConversionRequired`) and the conversion command rewrites it.


-- `body_format`: 1 is the only value a running build accepts.
-- `header_rev`: incremented in SQL by every header write (never computed by a client).
-- `message_count`: stored messages, System ones included.
-- `config_len`: UTF-8 length of the configuration with its bodies resolved.
-- `messages_len` / `checkpoints_len` (v31) keep their meaning: the length of the
-- resolved JSON document, which is what the learning size limit measures.
-- `non_restorable_reason`: projection of `config.non_restorable_reason` for listings.
ALTER TABLE run_contexts ADD COLUMN body_format INTEGER NOT NULL DEFAULT 1;
ALTER TABLE run_contexts ADD COLUMN header_rev INTEGER NOT NULL DEFAULT 0 CHECK (header_rev >= 0);
ALTER TABLE run_contexts ADD COLUMN message_count INTEGER NOT NULL DEFAULT 0 CHECK (message_count >= 0);
ALTER TABLE run_contexts ADD COLUMN config_len INTEGER NOT NULL DEFAULT 0 CHECK (config_len >= 0);
ALTER TABLE run_contexts ADD COLUMN non_restorable_reason TEXT;

-- One provider message per row. `ordinal` is the message's index in the saved array
-- (System messages included; the provenance ordinals skip them, these do not).
-- `msg_sha256` and `plain_len` describe the redacted message JSON before its long
-- strings became tokens; `skeleton` is that JSON with the tokens, plain (`codec` 0) or
-- one zstd frame (`codec` 1). `skeleton` is last so a change check reads the other
-- columns only.
CREATE TABLE run_messages (
    id         INTEGER PRIMARY KEY,
    run_id     TEXT NOT NULL REFERENCES run_contexts(run_id) ON DELETE NO ACTION,
    ordinal    INTEGER NOT NULL CHECK (ordinal >= 0),
    msg_sha256 BLOB NOT NULL CHECK (length(msg_sha256) = 32),
    plain_len  INTEGER NOT NULL CHECK (plain_len >= 0),
    codec      INTEGER NOT NULL CHECK (codec IN (0, 1)),
    skeleton   BLOB NOT NULL,
    UNIQUE (run_id, ordinal)
);

-- Nothing may be stored for a tombstoned run, and a row of one may not move to another.
CREATE TRIGGER fence_run_messages_insert BEFORE INSERT ON run_messages
WHEN NEW.run_id IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;
CREATE TRIGGER fence_run_messages_update BEFORE UPDATE ON run_messages
WHEN NEW.run_id IN (SELECT run_id FROM run_tombstones) OR OLD.run_id IN (SELECT run_id FROM run_tombstones)
BEGIN SELECT RAISE(ABORT, 'run or thread is tombstoned'); END;

-- Reference tables, in the shape `OwnerSpec::ref_table_ddl` produces (a test compares).
CREATE TABLE content_blob_refs_message (
    message_id INTEGER NOT NULL,
    blob_id INTEGER NOT NULL REFERENCES content_blobs(id),
    PRIMARY KEY (message_id, blob_id),
    FOREIGN KEY (message_id) REFERENCES run_messages(id) ON DELETE CASCADE
) WITHOUT ROWID;
CREATE INDEX idx_content_blob_refs_message_blob ON content_blob_refs_message(blob_id);
CREATE TRIGGER content_blob_refs_message_queue_gc AFTER DELETE ON content_blob_refs_message
BEGIN
    INSERT OR IGNORE INTO content_blob_gc_queue(blob_id) VALUES (OLD.blob_id);
END;

CREATE TABLE content_blob_refs_header (
    run_id TEXT NOT NULL,
    blob_id INTEGER NOT NULL REFERENCES content_blobs(id),
    PRIMARY KEY (run_id, blob_id),
    FOREIGN KEY (run_id) REFERENCES run_contexts(run_id) ON DELETE CASCADE
) WITHOUT ROWID;
CREATE INDEX idx_content_blob_refs_header_blob ON content_blob_refs_header(blob_id);
CREATE TRIGGER content_blob_refs_header_queue_gc AFTER DELETE ON content_blob_refs_header
BEGIN
    INSERT OR IGNORE INTO content_blob_gc_queue(blob_id) VALUES (OLD.blob_id);
END;

-- Replacing a message's skeleton drops the references of the old one in the same
-- statement; the writer registers the new set right after, in the same transaction.
CREATE TRIGGER run_messages_drop_refs BEFORE UPDATE OF skeleton ON run_messages
BEGIN
    DELETE FROM content_blob_refs_message WHERE message_id = OLD.id;
END;
