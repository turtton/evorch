-- `messages_json` / `checkpoints_json` may now hold a zstd BLOB (see codec.rs), so the
-- stored size no longer measures the snapshot. These columns record the plaintext
-- UTF-8 byte length, kept current by every write, for callers that need the size
-- without decompressing. Rows written before this migration are all plain TEXT.
ALTER TABLE run_contexts ADD COLUMN messages_len INTEGER NOT NULL DEFAULT 0;
ALTER TABLE run_contexts ADD COLUMN checkpoints_len INTEGER NOT NULL DEFAULT 0;
UPDATE run_contexts
SET messages_len = length(CAST(messages_json AS BLOB)),
    checkpoints_len = length(CAST(checkpoints_json AS BLOB));
