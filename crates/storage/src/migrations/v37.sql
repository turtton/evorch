-- Artifact blob collection (PR-5): resumable state of the collector.
--
-- `ref_cursor`: where the orphan-reference check stands.
-- `meta_cursor` (existing), `meta_count` and `meta_hash`: the marking pass over the
-- metadata directory may span several starts; the cursor is the last meta file name
-- looked at, and count / hash fingerprint the names up to it. A later start recomputes
-- the fingerprint from the directory and restarts the pass if it differs, so a meta
-- file added behind the cursor without a reference is never skipped.
ALTER TABLE gc_state ADD COLUMN ref_cursor TEXT;
ALTER TABLE gc_state ADD COLUMN meta_count INTEGER NOT NULL DEFAULT 0 CHECK (meta_count >= 0);
ALTER TABLE gc_state ADD COLUMN meta_hash TEXT NOT NULL DEFAULT '';
