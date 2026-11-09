-- Recovery identity is a destination and an immutable saved restore source,
-- never a claim, owner generation or Failed Report sequence.
CREATE TABLE lane_recovery_episodes (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    episode_id TEXT NOT NULL UNIQUE,
    destination_thread_id TEXT NOT NULL REFERENCES lane_delivery_destinations(thread_id) ON DELETE CASCADE,
    saved_root_run_id TEXT NOT NULL,
    stable_recovery_target TEXT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0 CHECK(attempts BETWEEN 0 AND 3),
    last_attempt_at_ns INTEGER,
    state TEXT NOT NULL CHECK(state IN ('retryable','suppressed','resolved')),
    last_failure_class TEXT,
    revision INTEGER NOT NULL CHECK(revision > 0),
    failure_report_ref TEXT,
    last_attempt_token TEXT
);
CREATE UNIQUE INDEX lane_recovery_active_destination
    ON lane_recovery_episodes(destination_thread_id) WHERE state <> 'resolved';
CREATE TRIGGER lane_recovery_thread_reclaim AFTER INSERT ON thread_tombstones BEGIN
    DELETE FROM lane_recovery_episodes WHERE destination_thread_id=NEW.thread_id;
END;
CREATE TRIGGER lane_recovery_root_reclaim AFTER INSERT ON run_tombstones BEGIN
    DELETE FROM lane_recovery_episodes WHERE saved_root_run_id=NEW.run_id;
END;
