-- Test-only old-schema reconstruction.
DROP TRIGGER lane_membership_epoch_insert;
DROP TRIGGER lane_management_list_invalidation;
DROP TRIGGER fence_lane_management_receipts_insert;
DROP TRIGGER lane_management_receipt_immutable;
DROP TABLE lane_management_receipts;
ALTER TABLE retention_staging_state DROP COLUMN family_topology_revision;
ALTER TABLE lane_state DROP COLUMN family_topology_revision;
ALTER TABLE lanes DROP COLUMN management_projection_revision;
ALTER TABLE lanes DROP COLUMN detached_since_management_revision;
