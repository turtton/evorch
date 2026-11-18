-- Renderer artifact metadata is paged only within an authorized thread.
-- One-time index construction reads O(artifact_refs rows); this is deliberate
-- migration work, not an unbounded read in a synchronous request handler.
CREATE INDEX idx_artifact_refs_thread_artifact
    ON artifact_refs(thread_id, artifact_id);
