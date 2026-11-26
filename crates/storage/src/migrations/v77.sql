-- Bounded online retention trigger targets (all states, not active-only).
CREATE INDEX pr_auto_fix_destination ON pr_auto_fix_state(destination_thread_id);
CREATE INDEX lane_recovery_destination ON lane_recovery_episodes(destination_thread_id);
CREATE INDEX lane_recovery_saved_root ON lane_recovery_episodes(saved_root_run_id);
-- An empty global protection probe must not scan all closed lanes.
CREATE INDEX lane_management_projection_pending ON lanes(thread_id)
 WHERE management_revision > 1 AND management_projection_revision < management_revision;
CREATE INDEX recovery_claim_members_run ON recovery_claim_members(run_id);
