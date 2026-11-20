-- Offline test fixture: drop the CLI continuity tables of v72 (their indexes and
-- triggers go with them).
DROP TABLE cli_continuity_clock;
DROP TABLE cli_continuity_grants;
