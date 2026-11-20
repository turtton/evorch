-- Offline test fixture: drop the administrative audit tables of v70 (their indexes
-- and triggers go with them).
DROP TABLE admin_denials;
DROP TABLE accepted_admin_operations;
