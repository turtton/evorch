DROP INDEX IF EXISTS pr_auto_fix_destination;
DROP INDEX IF EXISTS lane_recovery_destination;
DROP INDEX IF EXISTS lane_recovery_saved_root;
DROP INDEX IF EXISTS lane_management_projection_pending;
DROP INDEX IF EXISTS recovery_claim_members_run;
PRAGMA user_version = 75;
