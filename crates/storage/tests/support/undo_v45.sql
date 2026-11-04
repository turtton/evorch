-- Reverts migration v45 (the lane ledger's `start_blocked_reason` column), for tests that
-- rebuild an older schema by hand and replay the migrations from there. Apply it before
-- `undo_v44.sql`.
ALTER TABLE lanes DROP COLUMN start_blocked_reason;
