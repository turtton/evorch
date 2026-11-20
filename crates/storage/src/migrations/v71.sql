-- Q2 graph control is diagnostic durable state, never a persisted authority.
CREATE TABLE recovery_graph_control (
 manifest_id TEXT PRIMARY KEY NOT NULL REFERENCES recovery_manifests(manifest_id),
 mode TEXT NOT NULL CHECK(mode IN ('manual_running','manual_idle','revoked_running','revoked_idle','resolved')),
 revision INTEGER NOT NULL CHECK(revision > 0),
 operation_id TEXT NOT NULL,
 attempt_nonce TEXT NOT NULL,
 revoked INTEGER NOT NULL CHECK(revoked IN (0,1)),
 origin TEXT NOT NULL CHECK(origin IN ('manual_resume','manual_release','manual_release_without_claim'))
);
CREATE TRIGGER recovery_graph_revocation_permanent BEFORE UPDATE ON recovery_graph_control
WHEN OLD.revoked=1 AND NEW.revoked!=1
BEGIN SELECT RAISE(ABORT,'graph eligibility cannot be restored'); END;
CREATE TABLE recovery_graph_effects (
 operation_id TEXT PRIMARY KEY NOT NULL,
 attempt_nonce TEXT NOT NULL,
 manifest_id TEXT NOT NULL,
 request_json TEXT NOT NULL,
 result_json TEXT NOT NULL
);
-- One reset per graph disposal/thread/generation, independent of operation ID.
CREATE TABLE recovery_graph_thread_progress (
 disposal_key TEXT PRIMARY KEY NOT NULL,
 manifest_id TEXT NOT NULL,
 restart_id TEXT NOT NULL,
 capture_id TEXT NOT NULL,
 thread_id TEXT NOT NULL,
 owner_generation INTEGER NOT NULL,
 owners TEXT NOT NULL CHECK(owners IN ('not_started','applied','not_applicable','unknown')),
 scratch TEXT NOT NULL CHECK(scratch IN ('not_started','applied','not_applicable','unknown')),
 UNIQUE(manifest_id,restart_id,capture_id,thread_id,owner_generation)
);
