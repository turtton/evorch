//! Online retention: one seed, no staging/merging, no history estimate or owners.db.
//! Both preflight and confirmation call check() on their own consistent snapshot.
use super::closure::{Expansion, Neighbor, NodeKind, Position, expand_node};
use crate::thread_retention::{
    ConfirmedFamily, LegacyArchivedPolicy, NANOS_PER_DAY,
    decision::{self, DecisionBudget, FamilyMember, Verdict},
    online::{CommitFence, OnlineBudget, OnlineClosure, OnlineOutcome, OnlineRequest},
};
use crate::{Database, StorageError};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[cfg(test)]
#[path = "online_tests.rs"]
mod tests;

type Check<T> = Result<T, Error>;
enum Error {
    Hold(&'static str),
    Storage(StorageError),
}
impl From<StorageError> for Error {
    fn from(e: StorageError) -> Self {
        Self::Storage(e)
    }
}
impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Self::Storage(e.into())
    }
}
fn held<T>(reason: &'static str) -> Check<T> {
    Err(Error::Hold(reason))
}

/// Costs matching rows BEFORE filters/joins or trigger writes. LIMIT applies to
/// the indexed key, not to matching rows after an unbounded predicate. Shape
/// probes use octet_length so an oversized body is never fetched just to size it.
struct Meter<'a> {
    conn: &'a Connection,
    budget: OnlineBudget,
    rows: usize,
    bytes: u64,
    shapes: BTreeMap<&'static str, String>,
}
impl<'a> Meter<'a> {
    fn new(conn: &'a Connection, budget: OnlineBudget) -> Self {
        Self {
            conn,
            budget,
            rows: 0,
            bytes: 0,
            shapes: BTreeMap::new(),
        }
    }
    fn probe(&mut self, table: &'static str, key: &str, id: &str) -> Check<()> {
        let shape = if let Some(s) = self.shapes.get(table) {
            s.clone()
        } else {
            let mut s = self.conn.prepare(&format!("PRAGMA table_info({table})"))?;
            let columns = s
                .query_map([], |r| r.get::<_, String>(1))?
                .collect::<Result<Vec<_>, _>>()?;
            let shape = columns
                .iter()
                .map(|c| format!("COALESCE(octet_length(\"{c}\"),0)"))
                .collect::<Vec<_>>()
                .join("+");
            self.shapes.insert(table, shape.clone());
            shape
        };
        let mut s = self.conn.prepare_cached(&format!(
            "SELECT 128+{shape} FROM {table} WHERE {key}=?1 LIMIT ?2"
        ))?;
        let remaining = self.budget.side_effect_rows.saturating_sub(self.rows);
        let rows = s.query_map(params![id, remaining.saturating_add(1) as i64], |r| {
            r.get::<_, u64>(0)
        })?;
        for row in rows {
            self.rows += 1;
            self.bytes = self.bytes.saturating_add(row?);
            if self.rows > self.budget.side_effect_rows {
                return held("SideEffectRowBudget");
            }
            if self.bytes > self.budget.side_effect_bytes {
                return held("SideEffectByteBudget");
            }
        }
        Ok(())
    }
}
fn closure(conn: &Connection, seed: &str, budget: OnlineBudget) -> Check<OnlineClosure> {
    let deleted: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM thread_tombstones WHERE thread_id=?1)",
        [seed],
        |r| r.get(0),
    )?;
    if deleted {
        return held("ThreadDeleted");
    }
    let mut threads = BTreeSet::from([seed.to_owned()]);
    let mut runs = BTreeSet::new();
    let mut queue = VecDeque::from([(NodeKind::Thread, seed.to_owned())]);
    let mut edges = 0;
    let mut support = Meter::new(conn, budget);
    let mut manifests = BTreeSet::new();
    while let Some((kind, id)) = queue.pop_front() {
        // These source sets appear under filters/joins in expand_node. Bounding
        // only the emitted neighbors would not bound the rows SQLite inspected.
        if matches!(kind, NodeKind::Run) {
            for (table, key) in [
                ("recovery_members", "run_id"),
                ("usage_requests", "run_id"),
                ("usage_requests", "parent_run_id"),
                ("threads", "root_run_id"),
            ] {
                support.probe(table, key, &id)?;
            }
            // Bound the IN-subquery's complete target set before its ORDER BY.
            // LIMIT on emitted neighbors alone cannot bound a sorting query.
            let mut s = conn.prepare_cached(
                "SELECT manifest_id FROM recovery_members WHERE run_id=?1 LIMIT ?2",
            )?;
            let ids = s
                .query_map(
                    params![id, budget.side_effect_rows.saturating_add(1) as i64],
                    |r| r.get::<_, String>(0),
                )?
                .collect::<Result<Vec<_>, _>>()?;
            for manifest in ids {
                if manifests.insert(manifest.clone()) {
                    support.probe("recovery_members", "manifest_id", &manifest)?;
                }
            }
        }
        let mut hold = None;
        let outcome = expand_node(
            conn,
            kind,
            &id,
            Position::default(),
            &mut edges,
            budget.edges,
            &mut |neighbor| {
                let (kind, id, inserted) = match neighbor {
                    Neighbor::Gone(_) => {
                        hold = Some("FamilyUnresolved");
                        return Ok(false);
                    }
                    Neighbor::Thread(id) => (NodeKind::Thread, id.clone(), threads.insert(id)),
                    Neighbor::Run(id) => (NodeKind::Run, id.clone(), runs.insert(id)),
                };
                if threads.len() + runs.len() > budget.members.min(1_024) {
                    hold = Some("ClosureMemberBudget");
                    return Ok(false);
                }
                if id.len() as u64 > budget.side_effect_bytes {
                    hold = Some("SideEffectByteBudget");
                    return Ok(false);
                }
                if inserted {
                    queue.push_back((kind, id));
                }
                Ok(true)
            },
        )?;
        if let Some(reason) = hold {
            return held(reason);
        }
        if matches!(outcome, Expansion::Budget(_)) {
            return held("ClosureEdgeBudget");
        }
    }
    Ok(OnlineClosure {
        threads: threads.into_iter().collect(),
        runs: runs.into_iter().collect(),
    })
}

/// The closure discovery can be run before taking the host's canonical snapshot.
impl Database {
    pub fn retention_online_closure(
        &self,
        seed: &str,
        budget: OnlineBudget,
    ) -> Result<OnlineOutcome, StorageError> {
        let tx = self.conn.unchecked_transaction()?;
        result(closure(&tx, seed, budget).map(OnlineOutcome::Ready))
    }
    /// Read-only common preflight. The owner checks authority and durable root
    /// binding before calling, and never treats this answer as commit permission.
    pub fn retention_online_prepare(
        &self,
        request: &OnlineRequest,
        budget: OnlineBudget,
    ) -> Result<OnlineOutcome, StorageError> {
        let tx = self.conn.unchecked_transaction()?;
        result(check(&tx, request, budget).map(|(c, _)| OnlineOutcome::Ready(c)))
    }
}
fn result(r: Check<OnlineOutcome>) -> Result<OnlineOutcome, StorageError> {
    match r {
        Ok(o) => Ok(o),
        Err(Error::Hold(h)) => Ok(OnlineOutcome::Hold(h)),
        Err(Error::Storage(e)) => Err(e),
    }
}
fn check(
    conn: &Connection,
    request: &OnlineRequest,
    budget: OnlineBudget,
) -> Check<(OnlineClosure, Vec<String>)> {
    let OnlineRequest {
        db_identity,
        seed,
        expected,
        input,
        policy,
        binding,
    } = request;
    let actual_db_id: Option<String> = conn
        .query_row(
            "SELECT db_id FROM retention_identity WHERE id=1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if actual_db_id.as_deref() != Some(db_identity.as_str()) {
        return held("DatabaseIdentityMismatch");
    }
    if policy.archived_delete_after_days == 0 {
        return held("DeletionDisabled");
    }
    if crate::migrations::backfill::backfill_pending(conn)? {
        return held("HeaderBackfillPending");
    }
    let bound = super::binding(conn)?.is_some_and(|b| {
        b.ownership_root == binding.ownership_root && b.sidebar_path == binding.sidebar_path
    });
    if !bound
        || input.ownership_root != binding.ownership_root
        || input.sidebar_path != binding.sidebar_path
    {
        return held("BindingMismatch");
    }
    let actual = closure(conn, seed, budget)?;
    if actual != *expected {
        return held("ClosureChanged");
    }
    let Some(age) = i64::from(policy.archived_delete_after_days).checked_mul(NANOS_PER_DAY) else {
        return held("InvalidDeadline");
    };
    let Some(cutoff) = input.now_ns.checked_sub(age) else {
        return held("InvalidDeadline");
    };
    let sidebar: BTreeMap<_, _> = input
        .threads
        .iter()
        .map(|t| (t.thread_id.as_str(), t))
        .collect();
    let mut family = Vec::new();
    let mut meter = Meter::new(conn, budget);
    let pending_projection: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM lanes INDEXED BY lane_management_projection_pending WHERE management_revision>1 AND management_projection_revision<management_revision)", [], |r| r.get(0))?;
    if pending_projection {
        return held("MembershipProjectionPending");
    }
    for id in &actual.threads {
        let Some(t) = sidebar.get(id.as_str()) else {
            return held("SidebarMemberMissing");
        };
        if t.pinned {
            return held("Pinned");
        }
        if !t.archived {
            return held("MemberNotArchived");
        }
        let not_due = match t.archived_at_s {
            Some(at) => {
                at < 0
                    || at
                        .checked_mul(1_000_000_000)
                        .and_then(|ns| ns.checked_add(age))
                        .is_none_or(|due| input.now_ns < due)
            }
            None => {
                policy.legacy == LegacyArchivedPolicy::ByLastActivity
                    && t.updated_at_s
                        .checked_mul(1_000_000_000)
                        .is_none_or(|ns| ns > cutoff)
            }
        };
        if not_due {
            return held("MemberNotDue");
        }
        let Some(created_ns) = t.created_at_s.checked_mul(1_000_000_000) else {
            return held("InvalidDeadline");
        };
        family.push(FamilyMember {
            thread_id: id.clone(),
            created_ns,
            runs: vec![],
        });
        for (table, key) in [
            ("threads", "thread_id"),
            ("thread_runs", "thread_id"),
            ("pr_auto_fix_state", "destination_thread_id"),
            ("pr_notices", "destination_thread_id"),
            ("lane_recovery_episodes", "destination_thread_id"),
        ] {
            meter.probe(table, key, id)?;
        }
    }
    let mut tasks = BTreeSet::new();
    for run in &actual.runs {
        for table in [
            "restart_expected_members",
            "recovery_claim_members",
            "user_question_links",
            "user_questions",
        ] {
            meter.probe(table, "run_id", run)?;
        }
        let restart: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM restart_expected_members m JOIN restart_transactions t USING(restart_id) WHERE m.run_id=?1 AND t.phase!='completed')", [run], |r| r.get(0))?;
        if restart {
            return held("RestartEvidence");
        }
        let prepared: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM recovery_claim_members m JOIN recovery_claims c USING(claim_id) WHERE m.run_id=?1 AND c.state='prepared')", [run], |r| r.get(0))?;
        if prepared {
            return held("PreparedRecoveryClaim");
        }
        for (table, key) in [
            ("thread_runs", "run_id"),
            ("lane_handoffs", "source_run_id"),
            ("lane_recovery_episodes", "saved_root_run_id"),
        ] {
            meter.probe(table, key, run)?;
        }
        let mut s = conn.prepare_cached("SELECT id FROM tasks WHERE parent_run_id=?1 LIMIT ?2")?;
        let rows = s.query_map(
            params![
                run,
                budget.tasks.saturating_sub(tasks.len()).saturating_add(1) as i64
            ],
            |r| r.get::<_, String>(0),
        )?;
        for id in rows {
            let id = id?;
            meter.bytes = meter.bytes.saturating_add(id.len() as u64 + 128);
            tasks.insert(id);
            meter.rows += 1;
            if meter.rows > budget.side_effect_rows {
                return held("SideEffectRowBudget");
            }
            if tasks.len() > budget.tasks {
                return held("TaskBudget");
            }
            if meter.bytes > budget.side_effect_bytes {
                return held("SideEffectByteBudget");
            }
        }
    }
    meter.rows = meter
        .rows
        .saturating_add(actual.threads.len() * 2 + actual.runs.len() + 1);
    if meter.rows > budget.side_effect_rows {
        return held("SideEffectRowBudget");
    }
    let Some(first) = family.first_mut() else {
        return held("FamilyUnresolved");
    };
    // Plan C consumes the union, not a second run→thread graph.
    first.runs = actual.runs.clone();
    let decision =
        decision::decide_on_connection(conn, &family, cutoff, &DecisionBudget::default())?;
    if let Verdict::Hold(reason) = decision.verdict {
        return held(reason.reason());
    }
    Ok((actual, tasks.into_iter().collect()))
}
pub(crate) fn confirm(
    conn: &Connection,
    request: &OnlineRequest,
    budget: OnlineBudget,
    fence: &dyn CommitFence,
) -> Result<OnlineOutcome, StorageError> {
    // The first read is after taking the write lock. The queue wait is never sealed.
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let (actual, tasks) = match check(&tx, request, budget) {
        Ok(v) => v,
        Err(Error::Hold(h)) => return Ok(OnlineOutcome::Hold(h)),
        Err(Error::Storage(e)) => return Err(e),
    };
    let Some(mut seal) = fence.try_seal() else {
        return Ok(OnlineOutcome::Hold("AdmissionChanged"));
    };
    let family_key = request.seed.clone();
    super::confirm::write_tombstones(
        &tx,
        &family_key,
        &actual.threads,
        &actual.runs,
        &tasks,
        request.input.now_ns,
        0,
    )?;
    let confirmed = ConfirmedFamily {
        family_key,
        threads: actual.threads,
        runs: actual.runs,
        tasks,
        estimate_bytes: 0,
    };
    tx.commit()?;
    seal.publish(&confirmed);
    // publish precedes Drop even if the caller/receiver disappeared.
    drop(seal);
    Ok(OnlineOutcome::Confirmed(confirmed))
}
