-- Test-only reconstruction of v47, never a production downgrade.
DROP TABLE lane_forward_receipts;
DROP TABLE lane_closes;
DROP TABLE lane_verifications;
DROP TABLE lane_stall_episodes;
DROP TRIGGER lane_brief_invalidation;
DROP TRIGGER lane_management_invalidation;
DROP TRIGGER lane_phase2_monotonic;
ALTER TABLE lanes DROP COLUMN current_close_id;
ALTER TABLE lanes DROP COLUMN progress_epoch;
ALTER TABLE lanes DROP COLUMN request_revision;
