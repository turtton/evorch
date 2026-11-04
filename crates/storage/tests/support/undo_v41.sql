-- Removes the lane ledger migration v41 added (its tables and the lineage trigger; the
-- rebuilt `threads` table is a superset of the old one and stays), for tests that take a
-- converted database back to version 40. Apply it after `undo_v42.sql`.
DROP TABLE IF EXISTS lane_list_snapshot_rows;
DROP TABLE IF EXISTS lane_list_snapshots;
DROP TABLE IF EXISTS lane_delivery_keys;
DROP TABLE IF EXISTS lane_reports;
DROP TABLE IF EXISTS lanes;
DROP TABLE IF EXISTS lane_state;
DROP TRIGGER IF EXISTS threads_lane_lineage_sticky;
