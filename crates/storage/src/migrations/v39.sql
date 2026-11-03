-- Content blob foundation (PR A2a): content-addressed storage for long strings.
--
-- `hash` is the SHA-256 of the string's UTF-8 bytes and `byte_len` its length.
-- `codec` 0 stores the bytes as they are, 1 stores them as one zstd frame (only when
-- that is smaller). `data` is the last column so a row's other columns stay on the
-- first page. The table is not WITHOUT ROWID: `id` is the key every reference table
-- points at, and a row is large.
--
-- Nothing here reads or converts an existing row. The reference tables
-- (`content_blob_refs_*`) belong to their owners and are created with them.
CREATE TABLE content_blobs (
    id       INTEGER PRIMARY KEY,
    hash     BLOB NOT NULL UNIQUE CHECK (length(hash) = 32),
    byte_len INTEGER NOT NULL CHECK (byte_len >= 0),
    codec    INTEGER NOT NULL CHECK (codec IN (0, 1)),
    data     BLOB NOT NULL
);

-- Blobs whose last reference may be gone, filled by the AFTER DELETE trigger of every
-- reference table and drained by the writer's maintenance tick. It is a set of
-- candidates, not of garbage: an entry can name a blob that was referenced again or
-- deleted already, and the collector re-checks every reference in the statement that
-- deletes.
CREATE TABLE content_blob_gc_queue (
    blob_id INTEGER PRIMARY KEY
);
