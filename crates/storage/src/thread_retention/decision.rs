//! Canonical-source archive decision: nothing derived is persisted.
//!
//! One call judges one family from the source tables inside one read transaction.
//! Every `events` statement pins its index with `INDEXED BY`, so a planner change
//! cannot turn a probe into a scan, and every budget below turns an unbounded read
//! into a hold. A hold is always the safe direction: the family stays unarchived.
//!
//! What the decision does not cover is documented in `docs/storage-maintenance.md`:
//! a change committed after the read transaction began, an activity attribution that
//! only the payload (not the `run_id` header) carries, and a clock set back by more
//! than the archive age.

use crate::{Database, StorageError};
use event_bus::{
    AgentRunPhase, EventKind, LifecycleEvent, OrchestratorEvent, ThreadGoalPhase,
    ThreadGoalSnapshot,
};
use rusqlite::{Connection, params};
use std::collections::{BTreeMap, BTreeSet};

#[doc(hidden)]
pub mod sql {
    pub const RECENT_RUNS: &str = "SELECT j.value FROM json_each(?1) j WHERE \
        (SELECT e.wall_clock_ns FROM events e INDEXED BY idx_events_run_id \
         WHERE e.run_id = j.value ORDER BY e.id DESC LIMIT 1) > ?2";
    pub const RUN_TAIL: &str = "SELECT id, kind, subkind, wall_clock_ns, typeof(payload), \
        octet_length(payload) FROM events INDEXED BY idx_events_run_id \
        WHERE run_id = ?1 ORDER BY id DESC LIMIT ?2";
    pub const GOAL_LATEST: &str = "SELECT j.value, \
        (SELECT g.id FROM events g INDEXED BY idx_events_goal_by_run \
         WHERE g.run_id = j.value AND g.kind = 'Orchestrator' AND g.subkind = 'ThreadGoalUpdated' \
         ORDER BY g.id DESC LIMIT 1) FROM json_each(?1) j";
    pub const EVENT_SHAPE: &str =
        "SELECT typeof(payload), octet_length(payload) FROM events WHERE id = ?1";
    pub const EVENT_BODY: &str = "SELECT payload FROM events WHERE id = ?1";
    pub const RECENT_RECEIPTS: &str = "SELECT j.value FROM json_each(?1) j WHERE \
        EXISTS(SELECT 1 FROM host_input_receipts r INDEXED BY host_input_receipts_run_accepted \
               WHERE r.run_id = j.value AND r.accepted_at_ns > ?2) \
        OR EXISTS(SELECT 1 FROM host_chat_submissions s INDEXED BY host_chat_submissions_run_accepted \
               WHERE s.run_id = j.value AND s.accepted_at_ns > ?2)";
    pub const LEGACY_FALLBACK: &str = "SELECT j.value FROM json_each(?1) j \
        JOIN run_contexts c ON c.run_id = j.value \
        WHERE c.message_count > 0 AND c.updated_at_ns > ?2 \
        AND NOT EXISTS(SELECT 1 FROM events e INDEXED BY idx_events_run_id WHERE e.run_id = j.value) \
        AND NOT EXISTS(SELECT 1 FROM host_input_receipts r INDEXED BY host_input_receipts_run_accepted WHERE r.run_id = j.value) \
        AND NOT EXISTS(SELECT 1 FROM host_chat_submissions s INDEXED BY host_chat_submissions_run_accepted WHERE s.run_id = j.value)";
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
    pub const THREAD_RUNS: &str = "SELECT t.run_id FROM json_each(?1) j \
        JOIN thread_runs t INDEXED BY idx_thread_runs_thread ON t.thread_id = j.value LIMIT ?2";
    pub const INDEX_DDL: [&str; 3] = [
        "CREATE INDEX idx_events_goal_by_run ON events(run_id, id) \
         WHERE kind = 'Orchestrator' AND subkind = 'ThreadGoalUpdated' AND run_id IS NOT NULL",
        "CREATE INDEX host_input_receipts_run_accepted ON host_input_receipts(run_id, accepted_at_ns)",
        "CREATE INDEX host_chat_submissions_run_accepted ON host_chat_submissions(run_id, accepted_at_ns)",
    ];
}

/// Family threads plus runs a decision may inspect (`R`).
pub const FAMILY_ENTRY_LIMIT: usize = 1_024;
/// Newest events read for one run (`P`).
pub const TAIL_ROWS_PER_RUN: usize = 16;
/// Newest events read for one decision (`Q`).
pub const TAIL_ROWS_PER_DECISION: usize = 512;
/// Goal snapshots decoded for one decision (`K`).
pub const GOAL_DECODES_PER_DECISION: usize = 128;
/// Bytes read to be decoded, over activity, goal and handoff bodies.
pub const DECODE_BYTES_PER_DECISION: usize = 2_097_152;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecisionBudget {
    pub family_entries: usize,
    pub tail_rows_per_run: usize,
    pub tail_rows: usize,
    pub goal_decodes: usize,
    pub decode_bytes: usize,
}
impl Default for DecisionBudget {
    fn default() -> Self {
        Self {
            family_entries: FAMILY_ENTRY_LIMIT,
            tail_rows_per_run: TAIL_ROWS_PER_RUN,
            tail_rows: TAIL_ROWS_PER_DECISION,
            goal_decodes: GOAL_DECODES_PER_DECISION,
            decode_bytes: DECODE_BYTES_PER_DECISION,
        }
    }
}

/// One thread of a sidebar family: its creation time and the runs the sidebar binds to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FamilyMember {
    pub thread_id: String,
    pub created_ns: i64,
    pub runs: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hold {
    /// A member was created after the cutoff.
    NotDue,
    HeaderBackfillPending,
    FamilyRunBudget,
    EvaluationSample,
    UnansweredQuestion,
    OpenManagedLane,
    FamilyUnresolved,
    RecentActivity,
    ActivityProbeBudget,
    ActivityDecodeFailed,
    RecentInput,
    /// The only evidence is a recent save of a stored conversation.
    LegacyActivityFallback,
    UnfinishedGoal,
    GoalDecodeBudget,
    GoalUndecodable,
    GoalOutsideFamily,
}
impl Hold {
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::NotDue => "NotDue",
            Self::HeaderBackfillPending => "HeaderBackfillPending",
            Self::FamilyRunBudget => "FamilyRunBudget",
            Self::EvaluationSample => "EvaluationSample",
            Self::UnansweredQuestion => "UnansweredQuestion",
            Self::OpenManagedLane => "OpenManagedLane",
            Self::FamilyUnresolved => "FamilyUnresolved",
            Self::RecentActivity => "RecentActivity",
            Self::ActivityProbeBudget => "ActivityProbeBudget",
            Self::ActivityDecodeFailed => "ActivityDecodeFailed",
            Self::RecentInput => "RecentInput",
            Self::LegacyActivityFallback => "LegacyActivityFallback",
            Self::UnfinishedGoal => "UnfinishedGoal",
            Self::GoalDecodeBudget => "GoalDecodeBudget",
            Self::GoalUndecodable => "GoalUndecodable",
            Self::GoalOutsideFamily => "GoalOutsideFamily",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Due,
    Hold(Hold),
}

/// What one decision read, for budgets and diagnostics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DecisionStats {
    pub statements: usize,
    pub tail_rows: usize,
    pub decode_bytes: usize,
    pub goal_decodes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    pub verdict: Verdict,
    pub stats: DecisionStats,
}

struct Reader<'a> {
    conn: &'a Connection,
    budget: &'a DecisionBudget,
    stats: DecisionStats,
}

fn first_text(r: &rusqlite::Row<'_>) -> rusqlite::Result<String> {
    r.get(0)
}

type Hit<T> = Result<T, Result<Hold, StorageError>>;

fn hold<T>(h: Hold) -> Hit<T> {
    Err(Ok(h))
}
fn fail<T, E: Into<StorageError>>(e: E) -> Hit<T> {
    Err(Err(e.into()))
}

impl Reader<'_> {
    fn values(&mut self, text: &'static str, json: &str, extra: Option<i64>) -> Hit<Vec<String>> {
        self.stats.statements += 1;
        let mut stmt = self.conn.prepare_cached(text).or_else(fail)?;
        let rows = match extra {
            Some(x) => stmt.query_map(params![json, x], first_text),
            None => stmt.query_map(params![json], first_text),
        }
        .or_else(fail)?;
        rows.collect::<Result<Vec<_>, _>>().or_else(fail)
    }
    fn charge(&mut self, bytes: i64, over: Hold) -> Hit<()> {
        self.stats.decode_bytes = self
            .stats
            .decode_bytes
            .saturating_add(usize::try_from(bytes).unwrap_or(usize::MAX));
        if self.stats.decode_bytes > self.budget.decode_bytes {
            return hold(over);
        }
        Ok(())
    }
    fn text_body(&mut self, id: i64) -> Hit<Option<String>> {
        self.stats.statements += 1;
        let mut stmt = self.conn.prepare_cached(sql::EVENT_BODY).or_else(fail)?;
        let mut rows = stmt.query([id]).or_else(fail)?;
        let Some(row) = rows.next().or_else(fail)? else {
            return Ok(None);
        };
        Ok(row.get::<_, String>(0).ok())
    }
    fn protections(&mut self, threads: &str, runs: &str) -> Hit<()> {
        if !self.values(sql::EVAL_SAMPLES, threads, None)?.is_empty() {
            return hold(Hold::EvaluationSample);
        }
        self.stats.statements += 1;
        let lanes: Vec<(String, bool, bool)> = {
            let mut stmt = self.conn.prepare_cached(sql::LANES).or_else(fail)?;
            let rows = stmt
                .query_map(params![threads], |r| {
                    let policy: String = r.get(1)?;
                    let closed: bool = r.get(2)?;
                    let unapplied: bool = r.get(3)?;
                    Ok((policy, closed, unapplied))
                })
                .or_else(fail)?;
            rows.collect::<Result<Vec<_>, _>>().or_else(fail)?
        };
        for (policy, closed, unapplied) in lanes {
            if unapplied {
                return hold(Hold::FamilyUnresolved);
            }
            if matches!(policy.as_str(), "attached" | "summary") && !closed {
                return hold(Hold::OpenManagedLane);
            }
        }
        if !self.values(sql::QUESTIONS, runs, None)?.is_empty() {
            return hold(Hold::UnansweredQuestion);
        }
        Ok(())
    }
    fn activity(&mut self, json: &str, cutoff: i64) -> Hit<()> {
        let recent = self.values(sql::RECENT_RUNS, json, Some(cutoff))?;
        for run in recent {
            self.stats.statements += 1;
            let limit = self.budget.tail_rows_per_run.saturating_add(1);
            let limit = i64::try_from(limit).unwrap_or(i64::MAX);
            let tail: Vec<(i64, String, Option<String>, i64, String, i64)> = {
                let mut stmt = self.conn.prepare_cached(sql::RUN_TAIL).or_else(fail)?;
                let mut rows = stmt.query(params![run, limit]).or_else(fail)?;
                let mut out = Vec::new();
                while let Some(row) = rows.next().or_else(fail)? {
                    let at: i64 = row.get(3).or_else(fail)?;
                    out.push((
                        row.get(0).or_else(fail)?,
                        row.get(1).or_else(fail)?,
                        row.get(2).or_else(fail)?,
                        at,
                        row.get(4).or_else(fail)?,
                        row.get(5).or_else(fail)?,
                    ));
                    if at <= cutoff {
                        break;
                    }
                }
                out
            };
            let mut seen = 0usize;
            for (id, kind, sub, at, ty, bytes) in tail {
                seen += 1;
                self.stats.tail_rows += 1;
                if at <= cutoff {
                    break;
                }
                match (kind.as_str(), sub.as_deref()) {
                    (
                        "Message",
                        Some("MessageCompleted" | "FinalResultPublished" | "MessagePart"),
                    )
                    | ("Lifecycle", Some("TurnCompleted"))
                    | ("AgentMessage", Some("Delivered")) => return hold(Hold::RecentActivity),
                    ("Lifecycle", Some("AgentRunStateChanged")) => {
                        if ty != "text" {
                            return hold(Hold::ActivityDecodeFailed);
                        }
                        self.charge(bytes, Hold::ActivityProbeBudget)?;
                        let body = self.text_body(id)?;
                        let Some(Ok(EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                            from,
                            to,
                            ..
                        }))) = body.map(|b| serde_json::from_str::<EventKind>(&b))
                        else {
                            return hold(Hold::ActivityDecodeFailed);
                        };
                        if from != to
                            && (from == AgentRunPhase::Running || to == AgentRunPhase::Running)
                        {
                            return hold(Hold::RecentActivity);
                        }
                    }
                    _ => {}
                }
                if seen > self.budget.tail_rows_per_run
                    || self.stats.tail_rows > self.budget.tail_rows
                {
                    return hold(Hold::ActivityProbeBudget);
                }
            }
        }
        if !self
            .values(sql::RECENT_RECEIPTS, json, Some(cutoff))?
            .is_empty()
        {
            return hold(Hold::RecentInput);
        }
        if !self
            .values(sql::LEGACY_FALLBACK, json, Some(cutoff))?
            .is_empty()
        {
            return hold(Hold::LegacyActivityFallback);
        }
        Ok(())
    }
    fn goals(&mut self, members: &[FamilyMember], json: &str) -> Hit<()> {
        self.stats.statements += 1;
        let latest: Vec<i64> = {
            let mut stmt = self.conn.prepare_cached(sql::GOAL_LATEST).or_else(fail)?;
            let rows = stmt
                .query_map(params![json], |r| r.get::<_, Option<i64>>(1))
                .or_else(fail)?;
            rows.collect::<Result<Vec<_>, _>>()
                .or_else(fail)?
                .into_iter()
                .flatten()
                .collect()
        };
        if latest.len() > self.budget.goal_decodes {
            return hold(Hold::GoalDecodeBudget);
        }
        let family: BTreeSet<&str> = members.iter().map(|m| m.thread_id.as_str()).collect();
        let mut current: BTreeMap<String, (i64, ThreadGoalSnapshot)> = BTreeMap::new();
        for id in latest {
            self.stats.goal_decodes += 1;
            self.stats.statements += 1;
            let (ty, bytes): (String, i64) = self
                .conn
                .prepare_cached(sql::EVENT_SHAPE)
                .or_else(fail)?
                .query_row([id], |r| Ok((r.get(0)?, r.get(1)?)))
                .or_else(fail)?;
            if ty != "text" {
                return hold(Hold::GoalUndecodable);
            }
            self.charge(bytes, Hold::GoalDecodeBudget)?;
            let Some(body) = self.text_body(id)? else {
                return hold(Hold::GoalUndecodable);
            };
            let Ok(EventKind::Orchestrator(OrchestratorEvent::ThreadGoalUpdated { snapshot })) =
                serde_json::from_str::<EventKind>(&body)
            else {
                return hold(Hold::GoalUndecodable);
            };
            if !family.contains(snapshot.thread_id.as_str()) {
                return hold(Hold::GoalOutsideFamily);
            }
            let slot = current.entry(snapshot.thread_id.clone());
            slot.and_modify(|old| {
                if old.0 < id {
                    *old = (id, snapshot.clone());
                }
            })
            .or_insert((id, snapshot));
        }
        for (_, (_, goal)) in current {
            if self.transferred_away(&goal)? {
                continue;
            }
            if goal.phase != ThreadGoalPhase::Complete {
                return hold(Hold::UnfinishedGoal);
            }
        }
        Ok(())
    }
    /// The goal left its thread when a committed handoff of this goal id left the root run.
    fn transferred_away(&mut self, goal: &ThreadGoalSnapshot) -> Hit<bool> {
        self.stats.statements += 1;
        let receipts: Vec<(i64, i64)> = {
            let mut stmt = self.conn.prepare_cached(sql::HANDOFF_SHAPE).or_else(fail)?;
            let rows = stmt
                .query_map([&goal.root_run_id], |r| Ok((r.get(0)?, r.get(1)?)))
                .or_else(fail)?;
            rows.collect::<Result<Vec<_>, _>>().or_else(fail)?
        };
        for (rowid, bytes) in receipts {
            self.charge(bytes, Hold::GoalDecodeBudget)?;
            self.stats.statements += 1;
            let body: String = self
                .conn
                .prepare_cached(sql::HANDOFF_BODY)
                .or_else(fail)?
                .query_row([rowid], |r| r.get(0))
                .or_else(fail)?;
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&body) else {
                return hold(Hold::GoalUndecodable);
            };
            if value.get("goal_id").and_then(serde_json::Value::as_str) == Some(&goal.goal_id) {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

impl Database {
    /// Judges one sidebar family in one read transaction. `cutoff_ns` is `now - age`:
    /// activity at or before it is old enough. Never decides on a protection that only
    /// process memory knows (live runs, the in-memory goal): the caller checks those.
    pub fn decide_family_archive(
        &self,
        family: &[FamilyMember],
        cutoff_ns: i64,
        budget: &DecisionBudget,
    ) -> Result<Decision, StorageError> {
        let tx = self.conn.unchecked_transaction()?;
        let mut reader = Reader {
            conn: &tx,
            budget,
            stats: DecisionStats::default(),
        };
        let verdict = match reader.run(family, cutoff_ns) {
            Ok(()) => Verdict::Due,
            Err(Ok(h)) => Verdict::Hold(h),
            Err(Err(e)) => return Err(e),
        };
        Ok(Decision {
            verdict,
            stats: reader.stats,
        })
    }
}

fn json<T: serde::Serialize + ?Sized>(value: &T) -> Hit<String> {
    serde_json::to_string(value).map_err(|e| Err(StorageError::Serialization(e.to_string())))
}

impl Reader<'_> {
    /// Thread and run ids of the family: the sidebar's bindings plus `thread_runs`.
    fn collect(&mut self, family: &[FamilyMember]) -> Hit<(String, Vec<String>)> {
        if family.is_empty() {
            return hold(Hold::FamilyUnresolved);
        }
        let threads: Vec<&str> = family.iter().map(|m| m.thread_id.as_str()).collect();
        let mut runs: BTreeSet<String> = family.iter().flat_map(|m| m.runs.clone()).collect();
        let limit = self.budget.family_entries;
        if threads.len().saturating_add(runs.len()) > limit {
            return hold(Hold::FamilyRunBudget);
        }
        let thread_json = json(&threads)?;
        let bound = self.values(
            sql::THREAD_RUNS,
            &thread_json,
            Some(i64::try_from(limit).unwrap_or(i64::MAX)),
        )?;
        runs.extend(bound);
        if threads.len().saturating_add(runs.len()) > limit {
            return hold(Hold::FamilyRunBudget);
        }
        Ok((thread_json, runs.into_iter().collect()))
    }
    fn run(&mut self, family: &[FamilyMember], cutoff: i64) -> Hit<()> {
        let (thread_json, runs) = self.collect(family)?;
        if family.iter().any(|m| m.created_ns > cutoff) {
            return hold(Hold::NotDue);
        }
        let run_json = json(&runs)?;
        if crate::migrations::backfill::backfill_pending(self.conn).or_else(fail)? {
            return hold(Hold::HeaderBackfillPending);
        }
        self.protections(&thread_json, &run_json)?;
        self.activity(&run_json, cutoff)?;
        self.goals(family, &run_json)
    }
}

impl Database {
    /// Run ids a decision of this family would inspect, or the budget hold. The caller
    /// registers its admission stamps on exactly these before it decides.
    pub fn family_runs(
        &self,
        family: &[FamilyMember],
        budget: &DecisionBudget,
    ) -> Result<Result<Vec<String>, Hold>, StorageError> {
        let mut reader = Reader {
            conn: &self.conn,
            budget,
            stats: DecisionStats::default(),
        };
        match reader.collect(family) {
            Ok((_, runs)) => Ok(Ok(runs)),
            Err(Ok(h)) => Ok(Err(h)),
            Err(Err(e)) => Err(e),
        }
    }
}

#[cfg(test)]
#[path = "decision_tests.rs"]
mod tests;
