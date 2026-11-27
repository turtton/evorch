-- Rebuild derived history for the tool-duration reducer; source events and schema stay unchanged.
UPDATE history_run_state SET phase=0,
 source=CASE WHEN source < 9223372036854775807 THEN source + 1 END,
 generation=CASE WHEN generation < 9223372036854775807 THEN generation + 1 END;
UPDATE history_control SET reducer_version=3,
 generation=CASE WHEN generation < 9223372036854775807 THEN generation + 1 END WHERE id=1;
PRAGMA user_version = 79;
