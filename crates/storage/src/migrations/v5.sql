CREATE TABLE team_ledger (
    team_id TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK(revision > 0),
    state TEXT NOT NULL,
    PRIMARY KEY(team_id, revision)
);
CREATE TRIGGER team_ledger_no_update BEFORE UPDATE ON team_ledger
BEGIN SELECT RAISE(ABORT, 'team ledger is append-only'); END;
CREATE TRIGGER team_ledger_no_delete BEFORE DELETE ON team_ledger
BEGIN SELECT RAISE(ABORT, 'team ledger is append-only'); END;
