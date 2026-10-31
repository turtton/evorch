//! Archived-thread retention: the storage foundation.
//!
//! A deletion is *confirmed* once, in one transaction, by writing tombstones
//! for every thread, run and task of a family (see
//! [`Database::retention_finalize`]). From then on readers no longer see the
//! family, writers are rejected ([`StorageError::Tombstoned`]) and the rows are
//! reclaimed in bounded steps ([`StorageHandle::reclaim_step`]). Nothing here
//! decides *when* to confirm: the caller supplies the sidebar-derived input and
//! must hold the exclusive locks that prove no other evorch process runs.

use std::collections::{BTreeMap, HashSet};

use event_bus::{EventKind, OrchestratorEvent};
use rusqlite::Connection;

use crate::{Database, StorageError};

/// Nanoseconds in one day.
pub const NANOS_PER_DAY: i64 = 86_400_000_000_000;

/// Events deleted by one reclaim transaction (rows, and stored bytes below).
pub const RECLAIM_EVENT_ROWS: usize = 500;
pub const RECLAIM_EVENT_BYTES: u64 = 4 * 1024 * 1024;
/// Rows deleted from a small table by one reclaim transaction.
pub const RECLAIM_ROWS: usize = 500;
/// Ledger / todo rows *scanned* by one reclaim transaction; the cursor advances
/// by rows scanned, not by rows matched.
pub const RECLAIM_SCAN_ROWS: usize = 2_000;
/// Index blocks (and their text) purged by one reclaim transaction.
pub const RECLAIM_INDEX_ROWS: usize = 200;
pub const RECLAIM_INDEX_TEXT_BYTES: u64 = 2 * 1024 * 1024;
/// A single row above this many stored bytes is not deleted by the bounded
/// step; it is registered for the exclusive startup phase instead.
pub const RECLAIM_ROW_BUDGET: u64 = 4 * 1024 * 1024;
/// Transactions per `reclaim_step` unless the caller says otherwise.
pub const DEFAULT_RECLAIM_TRANSACTIONS: u32 = 8;
/// Events converted or attributed per backfill step unless the caller says otherwise.
pub const DEFAULT_RETENTION_BACKFILL_ROWS: usize = 2_000;

/// How long an archived thread is kept, and what an archive without a time means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThreadRetentionPolicy {
    /// 0 confirms nothing new (mirrors and staging are still maintained).
    pub archived_delete_after_days: u32,
    pub legacy: LegacyArchivedPolicy,
}

impl Default for ThreadRetentionPolicy {
    fn default() -> Self {
        Self {
            archived_delete_after_days: 30,
            legacy: LegacyArchivedPolicy::Immediate,
        }
    }
}

/// How an archive without a recorded time is treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyArchivedPolicy {
    /// Due at once. Protections (pin, evaluation sample, owner, family) still apply.
    Immediate,
    /// Due after the retention period counted from the latest activity. Kept for tests.
    ByLastActivity,
}

/// One thread of the sidebar, which is the authority on archive and pin state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarThread {
    pub thread_id: String,
    pub archived: bool,
    /// Unix seconds; `None` for an archive recorded without a time (legacy).
    pub archived_at_s: Option<i64>,
    pub pinned: bool,
    pub updated_at_s: i64,
    pub created_at_s: i64,
    pub archive_revision: Option<u64>,
    pub pin_revision: Option<u64>,
}

/// What the caller read from the sidebar once it held the exclusive locks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionInput {
    pub threads: Vec<SidebarThread>,
    pub sidebar_path: String,
    pub ownership_root: String,
    pub now_ns: i64,
}

/// Where a thread stands, for display and for the startup decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetentionStatus {
    NotArchived,
    Disabled,
    /// Whole days left (rounded up).
    DueIn {
        days: u32,
    },
    DueNextStartup,
    Blocked(BlockReason),
}

/// Why a thread or family is not confirmed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockReason {
    Pinned,
    EvalSample,
    FamilyMemberNotDue(String),
    FamilyMemberBlocked(String),
    FamilyUnresolved(String),
    /// An owner (a running or recently running conversation) holds the thread.
    Owner(String),
    /// Closure search or event backfill is not finished.
    Exploring,
    OverBudget,
}

/// Rows read by one `retention_explore` call. Every edge query is limited to the
/// budget that is left and an interrupted node resumes from the position it
/// reached. Two walks are indivisible and are charged in full once started: a
/// run's canonical-thread walk (at most 65 links) and a thread's lineage check
/// (at most 65 parent rows). A walk starts only while budget is left, so a call
/// reads at most 65 rows more than `max_edges`; the next call resumes from the
/// stored position, so repeated calls always finish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExploreBudget {
    pub max_edges: u64,
}

/// Progress of one family's closure search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FamilyState {
    Exploring {
        explored_edges: u64,
    },
    Explored {
        threads: Vec<String>,
        runs: Vec<String>,
    },
    Blocked(BlockReason),
}

/// Rows read by the fixed-point check of `retention_finalize`.
///
/// The check re-reads the relations the exploration read, so a family that has
/// not changed needs what the exploration used: [`Database::retention_finalize_budget`]
/// returns exactly that (plus slack). A family that grew may need more; then
/// `FinalizeOutcome::OverBudget` reports the rows read, and a retry with
/// [`FinalizeBudget::doubled`] converges because the family is finite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FinalizeBudget {
    pub max_edges: u64,
}

impl FinalizeBudget {
    /// Twice this budget, for the retry after `FinalizeOutcome::OverBudget`.
    #[must_use]
    pub fn doubled(self) -> Self {
        Self {
            max_edges: self.max_edges.saturating_mul(2).max(1024),
        }
    }
}

/// The ownership root and sidebar the first confirmation is bound to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingInput {
    pub ownership_root: String,
    pub sidebar_path: String,
}

/// The binding recorded by the first confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionBinding {
    pub ownership_root: String,
    pub sidebar_path: String,
    pub bound_at_ns: i64,
}

/// A family whose deletion was just confirmed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmedFamily {
    pub family_key: String,
    pub threads: Vec<String>,
    pub runs: Vec<String>,
    pub tasks: Vec<String>,
    /// Stored bytes of the family's events and run contexts: a measurement, not a guarantee.
    pub estimate_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinalizeOutcome {
    Confirmed(ConfirmedFamily),
    /// The family grew since it was explored; the new members were staged for
    /// another exploration. Nothing was confirmed.
    Stale {
        new_members: u64,
    },
    /// The fixed-point check needed more rows than the budget; nothing changed.
    OverBudget {
        members: u64,
        edges_read: u64,
    },
    Blocked(BlockReason),
    Disabled,
}

/// Transactions one `reclaim_step` may commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReclaimBudget {
    pub max_transactions: u32,
}

impl Default for ReclaimBudget {
    fn default() -> Self {
        Self {
            max_transactions: DEFAULT_RECLAIM_TRANSACTIONS,
        }
    }
}

/// What a reclaim step did. The `jobs_*` and `oversized_pending` counts saturate at
/// 1,000, so reading them costs the same however many jobs or registrations exist.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReclaimProgress {
    pub transactions: u32,
    pub rows: u64,
    /// Stored bytes of the deleted rows (events, run contexts, index text, ...).
    pub stored_bytes: u64,
    /// Jobs a further step can advance.
    pub jobs_db_pending: u64,
    /// Jobs whose bounded walk is over and that wait for `reclaim_oversized_step`.
    pub jobs_waiting_exclusive: u64,
    pub jobs_done: u64,
    /// Rows above [`RECLAIM_ROW_BUDGET`] waiting for `reclaim_oversized_step`.
    pub oversized_pending: u64,
    pub last_error: Option<String>,
}

/// What one `reclaim_oversized_step` did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OversizedProgress {
    pub rows: u64,
    pub stored_bytes: u64,
    /// Registrations left, saturating at 1,000 (zero means none).
    pub remaining: u64,
    /// The failure that stopped the step; the row stays registered.
    pub last_error: Option<String>,
}

/// The outcome of removing a family's files, reported by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilesOutcome {
    Pending,
    Done,
    Failed,
}

/// Reclaim bookkeeping at a glance.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReclaimSummary {
    pub jobs_db_pending: u64,
    pub jobs_db_done: u64,
    pub jobs_files_pending: u64,
    pub jobs_files_failed: u64,
    pub jobs_done: u64,
    /// Estimated stored bytes of the jobs still reclaiming rows.
    pub estimate_bytes_remaining: u64,
    pub oversized_rows: u64,
    pub oversized_bytes: u64,
}

/// Candidate overrides the caller adds to `retention_schedule`.
pub type BlockOverrides = BTreeMap<String, BlockReason>;

/// The identities removed by confirmed deletions.
///
/// The writer keeps one in memory so an append can be refused without a query
/// (see [`crate::StorageHandle`]); the set is small, as it only holds
/// families whose rows are still being reclaimed or whose tombstones remain.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TombstoneSet {
    pub threads: HashSet<String>,
    pub runs: HashSet<String>,
    pub tasks: HashSet<String>,
}

impl TombstoneSet {
    pub(crate) fn load(conn: &Connection) -> Result<Self, StorageError> {
        let column = |sql: &str| -> Result<HashSet<String>, StorageError> {
            let mut statement = conn.prepare(sql)?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            Ok(rows.collect::<Result<_, _>>()?)
        };
        Ok(Self {
            threads: column("SELECT thread_id FROM thread_tombstones")?,
            runs: column("SELECT run_id FROM run_tombstones")?,
            tasks: column("SELECT task_id FROM task_tombstones")?,
        })
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.threads.is_empty() && self.runs.is_empty() && self.tasks.is_empty()
    }

    #[must_use]
    pub fn contains_run(&self, run_id: &str) -> bool {
        self.runs.contains(run_id)
    }

    #[must_use]
    pub fn contains_thread(&self, thread_id: &str) -> bool {
        self.threads.contains(thread_id)
    }

    /// The tombstoned identity an event belongs to, if any: its owning run, or
    /// the thread of a thread goal / todo snapshot.
    pub(crate) fn rejects(&self, kind: &EventKind) -> Option<StorageError> {
        if self.runs.is_empty() && self.threads.is_empty() {
            return None;
        }
        let run_id = kind.primary_run_id().filter(|run| self.contains_run(run));
        let thread_id = match kind {
            EventKind::Orchestrator(OrchestratorEvent::ThreadGoalUpdated { snapshot }) => {
                Some(snapshot.thread_id.as_str())
            }
            EventKind::Orchestrator(OrchestratorEvent::ThreadTodoUpdated { snapshot }) => {
                Some(snapshot.thread_id.as_str())
            }
            _ => None,
        }
        .filter(|thread| self.contains_thread(thread));
        (run_id.is_some() || thread_id.is_some()).then(|| StorageError::Tombstoned {
            run_id: run_id.map(str::to_owned),
            thread_id: thread_id.map(str::to_owned),
        })
    }
}

impl Database {
    /// Status of every sidebar thread for display (hover text, ordering). Only
    /// archived, unpinned threads cost a database lookup.
    ///
    /// # Errors
    /// Returns an error if SQLite access fails.
    pub fn retention_schedule(
        &self,
        input: &RetentionInput,
        policy: &ThreadRetentionPolicy,
        overrides: &BlockOverrides,
    ) -> Result<Vec<(String, RetentionStatus)>, StorageError> {
        crate::repo::thread_retention::schedule(&self.conn, input, policy, overrides)
    }

    /// Advance the closure search of every candidate family by at most
    /// `budget.max_edges` rows. Uses this handle's single connection: the caller
    /// must hold the exclusive locks that prove no other evorch process runs.
    ///
    /// # Errors
    /// Returns an error if SQLite access fails.
    pub fn retention_explore(
        &self,
        input: &RetentionInput,
        policy: &ThreadRetentionPolicy,
        budget: ExploreBudget,
    ) -> Result<Vec<(String, FamilyState)>, StorageError> {
        crate::repo::thread_retention::explore(&self.conn, input, policy, budget)
    }

    /// Re-check the family's closure and protections and confirm its deletion in
    /// one transaction. `owner_blocks` maps thread IDs held by an owner to the
    /// reason. Same exclusivity requirement as [`Self::retention_explore`]; use
    /// [`Self::retention_finalize_budget`] for the first budget and
    /// [`FinalizeBudget::doubled`] to retry after `OverBudget`.
    ///
    /// # Errors
    /// Returns an error if SQLite access fails; nothing is confirmed then.
    pub fn retention_finalize(
        &self,
        family_key: &str,
        input: &RetentionInput,
        policy: &ThreadRetentionPolicy,
        owner_blocks: &BTreeMap<String, String>,
        binding: &BindingInput,
        budget: FinalizeBudget,
    ) -> Result<FinalizeOutcome, StorageError> {
        crate::repo::thread_retention::finalize(
            &self.conn,
            family_key,
            input,
            policy,
            owner_blocks,
            binding,
            budget,
        )
    }

    /// The budget that lets the fixed-point check of an unchanged explored family
    /// finish: the rows its exploration read, with a quarter and 1,024 rows of slack.
    /// `None` when the family is not staged.
    ///
    /// # Errors
    /// Returns an error if SQLite access fails.
    pub fn retention_finalize_budget(
        &self,
        family_key: &str,
    ) -> Result<Option<FinalizeBudget>, StorageError> {
        crate::repo::thread_retention::finalize_budget(&self.conn, family_key)
    }

    /// Delete rows too large for a bounded step, smallest first, at least one per
    /// call while any is waiting, until about `budget_bytes` are gone. Same
    /// exclusivity requirement as [`Self::retention_explore`].
    ///
    /// # Errors
    /// Returns an error if SQLite access fails.
    pub fn reclaim_oversized_step(
        &self,
        budget_bytes: u64,
    ) -> Result<OversizedProgress, StorageError> {
        crate::repo::thread_retention::reclaim::reclaim_oversized_step(&self.conn, budget_bytes)
    }

    /// Counts of reclaim jobs and waiting oversized rows.
    ///
    /// # Errors
    /// Returns an error if SQLite access fails.
    pub fn reclaim_summary(&self) -> Result<ReclaimSummary, StorageError> {
        crate::repo::thread_retention::reclaim::summary(&self.conn)
    }

    /// The ownership root and sidebar the first confirmation recorded.
    ///
    /// # Errors
    /// Returns an error if SQLite access fails.
    pub fn retention_binding(&self) -> Result<Option<RetentionBinding>, StorageError> {
        crate::repo::thread_retention::binding(&self.conn)
    }

    /// The random ID of this database, created on first use in its own
    /// transaction. The caller stores it next to the ownership root to detect a
    /// root that belongs to another database.
    ///
    /// # Errors
    /// Returns an error if SQLite access fails.
    pub fn retention_identity(&self) -> Result<String, StorageError> {
        crate::repo::thread_retention::identity(&self.conn)
    }

    /// Every tombstoned identity. Small by construction; read it once and test
    /// membership instead of querying per item.
    ///
    /// # Errors
    /// Returns an error if SQLite access fails.
    pub fn tombstones(&self) -> Result<TombstoneSet, StorageError> {
        TombstoneSet::load(&self.conn)
    }
}
