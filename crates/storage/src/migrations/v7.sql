CREATE TRIGGER run_ledger_no_replace BEFORE INSERT ON run_ledger
WHEN NEW.seq IS NOT NULL AND EXISTS (SELECT 1 FROM run_ledger WHERE seq = NEW.seq)
BEGIN SELECT RAISE(ABORT, 'run ledger is append-only'); END;
