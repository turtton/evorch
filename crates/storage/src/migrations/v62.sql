-- Restart evidence is deliberately independent of manifests and run tombstones.
-- Failed partial captures must retain the full lane/scan exclusion set.
CREATE TABLE restart_transactions (
 restart_id TEXT PRIMARY KEY, capture_id TEXT UNIQUE NOT NULL,
 revision INTEGER NOT NULL CHECK(revision >= 0), phase TEXT NOT NULL,
 metadata_json TEXT NOT NULL,
 old_effective_tools_fingerprint TEXT, new_effective_tools_fingerprint TEXT,
 old_compiled_catalog_fingerprint TEXT, new_compiled_catalog_fingerprint TEXT,
 old_builtin_tool_names_json TEXT NOT NULL,
 tools_attribution TEXT, tools_boundary_reason TEXT, tools_boundary_consumption_json TEXT,
 expected_digest TEXT, manifest_digest TEXT, ticket_nonce_hash TEXT,
 adopted_epoch INTEGER, ready_at_ms INTEGER, failure_reason TEXT
);
CREATE TABLE restart_expected_members (
 restart_id TEXT NOT NULL REFERENCES restart_transactions(restart_id),
 run_id TEXT NOT NULL, root_run_id TEXT NOT NULL, member_json TEXT NOT NULL,
 manifest_id TEXT,
 PRIMARY KEY(restart_id,run_id)
);
CREATE INDEX restart_expected_run ON restart_expected_members(run_id);
CREATE TABLE restart_components (
 restart_id TEXT NOT NULL REFERENCES restart_transactions(restart_id),
 manifest_id TEXT NOT NULL UNIQUE, disposition TEXT NOT NULL,
 claim_id TEXT, reason TEXT, revision INTEGER NOT NULL DEFAULT 0,
 PRIMARY KEY(restart_id,manifest_id)
);
CREATE TABLE restart_owner_progress (
 restart_id TEXT NOT NULL REFERENCES restart_transactions(restart_id),
 thread_id TEXT NOT NULL, progress_json TEXT NOT NULL,
 PRIMARY KEY(restart_id,thread_id)
);
-- A legacy writer cannot silently replace durable restart membership.
CREATE TRIGGER invalidate_restart_manifest_delete BEFORE DELETE ON recovery_manifests
BEGIN
 UPDATE restart_transactions SET phase='invalidated', revision=revision+1,
 failure_reason='manifest deleted'
 WHERE restart_id IN (SELECT restart_id FROM restart_components WHERE manifest_id=OLD.manifest_id)
 AND phase NOT IN ('completed','aborted','invalidated','superseded');
END;
CREATE TRIGGER invalidate_restart_manifest_supersede AFTER UPDATE OF superseded ON recovery_manifests
WHEN NEW.superseded=1 AND OLD.superseded=0
BEGIN
 UPDATE restart_transactions SET phase='superseded', revision=revision+1,
 failure_reason='manifest superseded'
 WHERE restart_id IN (SELECT restart_id FROM restart_components WHERE manifest_id=OLD.manifest_id AND disposition='retry_pending')
 AND phase NOT IN ('completed','aborted','invalidated','superseded');
END;
