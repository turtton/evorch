//! Canonical-source archive decision (plan C): nothing derived is persisted.
//!
//! Every statement here is a read over the source tables, with the events index
//! pinned by `INDEXED BY` so a planner change cannot turn a probe into a scan.
//! `sql` is public (hidden) only so the measurement example runs the exact text
//! the decision runs.

#[doc(hidden)]
pub mod sql {
    /// Runs (bound as one JSON array) whose newest event is after the cutoff.
    pub const RECENT_RUNS: &str = "SELECT j.value FROM json_each(?1) j WHERE \
        (SELECT e.wall_clock_ns FROM events e INDEXED BY idx_events_run_id \
         WHERE e.run_id = j.value ORDER BY e.id DESC LIMIT 1) > ?2";
    /// Newest events of one run, newest first (the caller stops at the cutoff).
    pub const RUN_TAIL: &str = "SELECT id, kind, subkind, wall_clock_ns, typeof(payload), \
        octet_length(payload) FROM events INDEXED BY idx_events_run_id \
        WHERE run_id = ?1 ORDER BY id DESC LIMIT ?2";
    /// Newest goal snapshot event of each run (NULL id: no snapshot).
    pub const GOAL_LATEST: &str = "SELECT j.value, \
        (SELECT g.id FROM events g INDEXED BY idx_events_goal_by_run \
         WHERE g.run_id = j.value AND g.kind = 'Orchestrator' AND g.subkind = 'ThreadGoalUpdated' \
         ORDER BY g.id DESC LIMIT 1) FROM json_each(?1) j";
    pub const EVENT_SHAPE: &str =
        "SELECT typeof(payload), octet_length(payload) FROM events WHERE id = ?1";
    pub const EVENT_BODY: &str = "SELECT payload FROM events WHERE id = ?1";
    /// Runs with a host input or chat submission accepted after the cutoff.
    pub const RECENT_RECEIPTS: &str = "SELECT j.value FROM json_each(?1) j WHERE \
        EXISTS(SELECT 1 FROM host_input_receipts r INDEXED BY host_input_receipts_run_accepted \
               WHERE r.run_id = j.value AND r.accepted_at_ns > ?2) \
        OR EXISTS(SELECT 1 FROM host_chat_submissions s INDEXED BY host_chat_submissions_run_accepted \
               WHERE s.run_id = j.value AND s.accepted_at_ns > ?2)";
    /// Runs with no event, no receipt and no submission but a saved conversation
    /// recently saved: their only evidence is the (delay-only) save time.
    pub const LEGACY_FALLBACK: &str = "SELECT j.value FROM json_each(?1) j \
        JOIN run_contexts c ON c.run_id = j.value \
        WHERE c.message_count > 0 AND c.updated_at_ns > ?2 \
        AND NOT EXISTS(SELECT 1 FROM events e INDEXED BY idx_events_run_id WHERE e.run_id = j.value) \
        AND NOT EXISTS(SELECT 1 FROM host_input_receipts r INDEXED BY host_input_receipts_run_accepted WHERE r.run_id = j.value) \
        AND NOT EXISTS(SELECT 1 FROM host_chat_submissions s INDEXED BY host_chat_submissions_run_accepted WHERE s.run_id = j.value)";
    /// Committed-or-later handoff receipts of a source run (the goal moved away).
    pub const HANDOFF_SHAPE: &str = "SELECT rowid, octet_length(goal_json) FROM lane_handoffs \
        WHERE source_run_id = ?1 AND goal_json IS NOT NULL \
        AND state IN ('committed', 'started', 'recoverable_start_failure')";
    pub const HANDOFF_BODY: &str = "SELECT goal_json FROM lane_handoffs WHERE rowid = ?1";
    pub const QUESTIONS: &str = "SELECT j.value FROM json_each(?1) j WHERE \
        EXISTS(SELECT 1 FROM user_questions q INDEXED BY user_questions_run WHERE q.run_id = j.value AND q.answered = 0) \
        OR EXISTS(SELECT 1 FROM user_questions q INDEXED BY idx_retention_questions_root_pending \
                  WHERE q.root_run_id = j.value AND q.answered = 0) \
        OR EXISTS(SELECT 1 FROM user_question_links l INDEXED BY user_question_links_run \
                  JOIN user_questions q ON q.id = l.question_id WHERE l.run_id = j.value AND q.answered = 0)";
    pub const EVAL_SAMPLES: &str = "SELECT j.value FROM json_each(?1) j \
        WHERE EXISTS(SELECT 1 FROM eval_samples s WHERE s.thread_id = j.value)";
    pub const LANES: &str = "SELECT l.thread_id, l.policy, \
        EXISTS(SELECT 1 FROM lane_closes c WHERE c.close_id = l.current_close_id AND c.lane_id = l.lane_id), \
        l.management_revision > 1 AND l.management_projection_revision < l.management_revision \
        FROM json_each(?1) j JOIN lanes l ON l.thread_id = j.value";
    /// The v75 indexes (also created by `v75.sql`).
    pub const INDEX_DDL: [&str; 3] = [
        "CREATE INDEX idx_events_goal_by_run ON events(run_id, id) \
         WHERE kind = 'Orchestrator' AND subkind = 'ThreadGoalUpdated' AND run_id IS NOT NULL",
        "CREATE INDEX host_input_receipts_run_accepted ON host_input_receipts(run_id, accepted_at_ns)",
        "CREATE INDEX host_chat_submissions_run_accepted ON host_chat_submissions(run_id, accepted_at_ns)",
    ];
}
