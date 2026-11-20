//! Q1: bounded quarantine inspection and the permanent discard of resume eligibility.
//!
//! A quarantined graph is a restart component whose recovery evidence was set
//! aside. Nothing here starts work, acquires an owner, rewrites history or
//! grants authority: a discard only records a permanent, revoked eligibility
//! row and releases an existing prepared claim. The two parts are observed
//! separately: `revoked` is the eligibility, `completion` is what is left to
//! clean up. A graph is only `Complete` when nothing is left.
use super::*;
use crate::writer::StorageHandle;

mod store;

/// Most graphs one list returns.
pub const QUARANTINE_LIST_CAP: usize = 32;
/// Most graphs one lookup inspects when it resolves an opaque reference.
pub const QUARANTINE_SCAN_CAP: usize = 1024;
/// Release reason recorded on a claim that a discard released.
pub const DISCARD_RELEASE_REASON: &str = "quarantine discard";
const COLD_REASON: &str = "cold boot has no inherited proof";
const RELEASED_WITHOUT_CLAIM_REASON: &str = "ManualReleaseWithoutClaim";

/// Why the evidence was set aside. Free text from the database is never exposed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuarantineReason {
    ColdBoot,
    ReleasedWithoutClaim,
    Other,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuarantineClaim {
    Absent,
    Prepared,
    Activated,
    Released,
}
/// What is left after the eligibility was revoked. Revocation alone is not completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuarantineCompletion {
    /// Eligibility is still open.
    NotStarted,
    /// Revoked, no claim is prepared, and every thread of the graph has a
    /// finished record for its owners and scratch (or the graph has no thread).
    Complete,
    /// Revoked, with a prepared claim, a disposal step, or a thread with no
    /// disposal record yet. A missing record is not evidence of nothing to clean.
    Partial,
    /// Revoked, but a recorded step is unknown or a claim is in an unexpected state.
    Unknown,
}
/// Raw identities stay on the server; the host exposes only opaque references.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuarantinedGraph {
    pub restart_id: String,
    pub capture_id: String,
    pub manifest_id: String,
    /// Monotone fold of every row revision this graph's state depends on.
    pub revision: u64,
    /// No live successor adopted the transaction: the graph has no owner to act for.
    pub cold: bool,
    pub terminal: bool,
    pub reason: QuarantineReason,
    pub claim: QuarantineClaim,
    pub revoked: bool,
    pub completion: QuarantineCompletion,
    pub members: u32,
    pub control: Option<graph_control::GraphControlRecord>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuarantineSnapshot {
    pub graphs: Vec<QuarantinedGraph>,
    /// More graphs exist than `limit`.
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscardRequest {
    pub restart_id: String,
    pub capture_id: String,
    pub manifest_id: String,
    pub expected_revision: u64,
    pub operation_id: String,
    pub attempt_nonce: String,
}
/// A definite refusal. Nothing was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscardRefusal {
    NotFound,
    /// The revision or the capture identity is not the expected one.
    Conflict,
    /// A live successor adopted the graph: its owner holds it, not this leaf.
    NotCold,
    /// The transaction is still running.
    NotTerminal,
    /// A manual resume holds the graph's eligibility.
    ManualInProgress,
    /// The operation ID was used for a different request.
    OperationConflict,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscardChange {
    /// This call recorded the revocation.
    Revoked,
    /// The graph was already revoked; this call changed no eligibility.
    AlreadyRevoked,
    /// The same operation had completed earlier; nothing was written.
    Replayed,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimRelease {
    NotNeeded,
    Released,
    /// The claim stayed prepared. Eligibility is revoked, cleanup is not finished.
    Failed,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscardResult {
    pub change: DiscardChange,
    pub claim_release: ClaimRelease,
    pub graph: QuarantinedGraph,
}
pub type DiscardReport = Result<DiscardResult, DiscardRefusal>;

impl Database {
    /// A bounded read of the quarantined graphs, newest first.
    ///
    /// # Errors
    /// SQLite read failures or an unreadable row.
    pub fn quarantine_snapshot(&self, limit: usize) -> Result<QuarantineSnapshot, StorageError> {
        let snapshot = self.conn.unchecked_transaction()?;
        store::snapshot(&snapshot, limit)
    }
    /// One graph by its raw identity, or `None` when it is not a quarantined graph.
    ///
    /// # Errors
    /// SQLite read failures or an unreadable row.
    pub fn quarantined_graph(
        &self,
        restart_id: &str,
        manifest_id: &str,
    ) -> Result<Option<QuarantinedGraph>, StorageError> {
        let snapshot = self.conn.unchecked_transaction()?;
        store::graph(&snapshot, restart_id, manifest_id)
    }
    /// Like [`Self::quarantine_snapshot`] on a fresh read-only connection. It does
    /// not migrate, open a writer or alter pragmas.
    ///
    /// # Errors
    /// Missing or non-current format, or SQLite read failures.
    pub fn read_quarantine_snapshot(
        config: &crate::StorageConfig,
        limit: usize,
    ) -> Result<QuarantineSnapshot, StorageError> {
        let conn = read_only(config)?;
        let tx = conn.unchecked_transaction()?;
        store::snapshot(&tx, limit)
    }
}
fn read_only(config: &crate::StorageConfig) -> Result<Connection, StorageError> {
    let conn = Connection::open_with_flags(
        &config.db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    crate::migrations::require_current_format(&conn)?;
    Ok(conn)
}

impl StorageHandle {
    /// Permanently revokes a cold quarantined graph's resume eligibility through
    /// the single writer, with a FULL-durable atomic transaction.
    ///
    /// The outer error means the writer's answer is unknown or the write failed:
    /// read the same operation back. It never creates an owner or touches
    /// history. Repeating one operation is safe.
    ///
    /// # Errors
    /// A closed writer, suspended writes or a SQLite failure.
    pub fn discard_quarantined_graph(
        &self,
        request: DiscardRequest,
    ) -> Result<DiscardReport, StorageError> {
        self.lane_job(true, move |conn| store::discard(conn, &request))
    }
}
