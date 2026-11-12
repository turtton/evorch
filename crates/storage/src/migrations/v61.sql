-- Graph receipts are evidence, never authority. Claims do not consume snapshots.
-- ADD COLUMN with CHECK would scan every saved intent. The typed revision API
-- validates values and quarantines unreadable rows; adding its default is O(1).
ALTER TABLE resume_intents ADD COLUMN revision INTEGER NOT NULL DEFAULT 0;
CREATE TRIGGER advance_resume_intent_revision AFTER UPDATE OF incarnation, payload ON resume_intents
BEGIN UPDATE resume_intents SET revision=OLD.revision+1 WHERE run_id=NEW.run_id; END;
CREATE TABLE recovery_manifests (
    manifest_id TEXT PRIMARY KEY NOT NULL,
    schema_version INTEGER NOT NULL,
    committed INTEGER NOT NULL DEFAULT 0 CHECK(committed IN (0,1)),
    superseded INTEGER NOT NULL DEFAULT 0 CHECK(superseded IN (0,1)),
    member_count INTEGER NOT NULL,
    edge_count INTEGER NOT NULL,
    edges_json TEXT NOT NULL
);
CREATE TABLE recovery_members (
    manifest_id TEXT NOT NULL REFERENCES recovery_manifests(manifest_id) ON DELETE CASCADE,
    run_id TEXT NOT NULL,
    incarnation TEXT NOT NULL,
    intent_revision INTEGER NOT NULL,
    snapshot_json TEXT,
    resume INTEGER NOT NULL CHECK(resume IN (0,1)),
    record_json TEXT NOT NULL,
    PRIMARY KEY(manifest_id,run_id)
);
CREATE INDEX recovery_members_run ON recovery_members(run_id);
CREATE TABLE recovery_claims (
    claim_id TEXT PRIMARY KEY NOT NULL,
    manifest_id TEXT NOT NULL REFERENCES recovery_manifests(manifest_id) ON DELETE CASCADE,
    state TEXT NOT NULL CHECK(state IN ('prepared','activated','released')),
    release_reason TEXT
);
CREATE UNIQUE INDEX recovery_claim_prepared ON recovery_claims(manifest_id) WHERE state IN ('prepared','activated');
CREATE TABLE recovery_claim_members (
    claim_id TEXT NOT NULL REFERENCES recovery_claims(claim_id) ON DELETE CASCADE,
    run_id TEXT NOT NULL,
    old_incarnation TEXT NOT NULL,
    reserved_revision INTEGER NOT NULL,
    new_incarnation TEXT NOT NULL,
    PRIMARY KEY(claim_id,run_id)
);
CREATE TRIGGER fence_recovery_member_insert BEFORE INSERT ON recovery_members
WHEN EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=NEW.run_id)
BEGIN SELECT RAISE(ABORT,'run is tombstoned'); END;
CREATE TRIGGER fence_recovery_member_update BEFORE UPDATE ON recovery_members
WHEN EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=NEW.run_id)
BEGIN SELECT RAISE(ABORT,'run is tombstoned'); END;
CREATE TRIGGER fence_recovery_claim_member_insert BEFORE INSERT ON recovery_claim_members
WHEN EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=NEW.run_id)
BEGIN SELECT RAISE(ABORT,'run is tombstoned'); END;
CREATE TRIGGER fence_recovery_claim_member_update BEFORE UPDATE ON recovery_claim_members
WHEN EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=NEW.run_id)
BEGIN SELECT RAISE(ABORT,'run is tombstoned'); END;
CREATE TRIGGER fence_recovery_manifest_commit BEFORE UPDATE ON recovery_manifests
WHEN NEW.committed=1 AND OLD.committed=0
AND EXISTS(SELECT 1 FROM recovery_members m JOIN run_tombstones t USING(run_id) WHERE m.manifest_id=NEW.manifest_id)
BEGIN SELECT RAISE(ABORT,'manifest member is tombstoned'); END;
CREATE TRIGGER fence_recovery_claim_update BEFORE UPDATE ON recovery_claims
WHEN EXISTS(SELECT 1 FROM recovery_claim_members m JOIN run_tombstones t USING(run_id) WHERE m.claim_id=NEW.claim_id)
BEGIN SELECT RAISE(ABORT,'claim member is tombstoned'); END;

-- A graph member can embed other members' deliveries. Erasure removes the whole
-- component, rather than retaining copies in a different member's JSON.
CREATE TRIGGER erase_recovery_component AFTER DELETE ON recovery_members
BEGIN DELETE FROM recovery_manifests WHERE manifest_id=OLD.manifest_id; END;
