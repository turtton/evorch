-- Test-only reconstruction of v45; never a production downgrade.
DROP TABLE lane_question_routes;
DROP TABLE lane_notifications;
DROP TABLE lane_delivery_destinations;
ALTER TABLE lanes DROP COLUMN wake_guard_json;
ALTER TABLE lanes DROP COLUMN question_capacity_blocked_json;
ALTER TABLE lane_reports DROP COLUMN artifact_revision_json;
ALTER TABLE lane_reports DROP COLUMN failure_episode;
ALTER TABLE host_input_receipts DROP COLUMN provenance_json;
