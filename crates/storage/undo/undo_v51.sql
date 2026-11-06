-- Test-only removal before reconstructing older fixtures; no production downgrade.
DROP TRIGGER pr_policy_link_ended;
DROP TRIGGER pr_policy_archived;
DROP TRIGGER pr_policy_archived_insert;
DROP TRIGGER pr_policy_deleted;
DROP TABLE pr_auto_fix_limit_events;
DROP TABLE pr_notices;
DROP TABLE pr_auto_fix_state;
