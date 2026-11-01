-- Artifact blob collection (PR-5): the position of the orphan-reference check.
--
-- `gc_state.meta_cursor` stays in the table but is no longer written: whether the
-- references are complete is decided from a full listing of the metadata directory
-- in the same startup (see `runtime::artifacts::gc`), never from a position kept
-- across starts.
ALTER TABLE gc_state ADD COLUMN ref_cursor TEXT;
