-- Handoff receipts are execution intents, not retention-family edges.
CREATE TABLE lane_handoffs (
    source_run_id TEXT NOT NULL,
    source_incarnation TEXT NOT NULL,
    target_run_id TEXT NOT NULL UNIQUE,
    lane_id TEXT NOT NULL UNIQUE,
    request_json TEXT NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('preparing','prepared','committed','started','recoverable_start_failure')),
    goal_json TEXT,
    todo_json TEXT,
    failure TEXT,
    UNIQUE(source_run_id,source_incarnation)
);
-- Ownership is per question, not per reusable source run. Its original
-- requester payload is immutable; a later Worker incarnation cannot take it.
CREATE TABLE lane_handoff_questions (
    question_id TEXT PRIMARY KEY REFERENCES user_questions(id) ON DELETE CASCADE,
    source_run_id TEXT NOT NULL,
    source_incarnation TEXT NOT NULL,
    target_run_id TEXT NOT NULL,
    committed INTEGER NOT NULL DEFAULT 0 CHECK(committed IN (0,1))
);
CREATE INDEX lane_handoff_questions_operation
    ON lane_handoff_questions(source_run_id,source_incarnation);
CREATE TRIGGER lane_handoff_reclaim AFTER DELETE ON lanes BEGIN
    DELETE FROM lane_handoffs WHERE lane_id=OLD.lane_id;
END;
