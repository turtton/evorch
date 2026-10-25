//! Signature-only contracts. SQL/writer implementations belong to later steps.

use serde::{Deserialize, Serialize};

use super::{
    ExtractedIndexInput, ExtractionError, GuardedReference, IndexInput, SnapshotBundle,
    SnapshotReceipt, SnapshotWrite,
};
use crate::{LimitKind, RunLedgerEntry, StorageError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexState {
    Pending,
    Indexed,
    RetryWait,
    Blocked,
    Evicted,
    Deleted,
}

/// Events for ADR R2's full matrix; discovery/config changes are not reindex.
/// Evicted stays evicted except explicit include-evicted reindex; deleted is a
/// permanent tombstone. Accounting-only changes must not re-extract indexed text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexEvent {
    DiscoverSameRevision,
    SettingsApplied,
    SourceUpdated,
    Reindex { include_evicted: bool },
    ExtractorChanged,
    AccountingChanged,
    RetryDue,
    Commit,
    TransientFailure,
    Blocked,
    CapacityPaused,
    Evict,
    DeleteSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", content = "detail", rename_all = "snake_case")]
pub enum IndexBlockedReason {
    Extraction(ExtractionError),
    AttemptsExhausted,
}

/// Identity shared by request targets and ordinary work. Request targets freeze
/// it at submission; revisions/versions must NEVER be overwritten in place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexTarget {
    pub run_id: String,
    pub source_revision: u64,
    pub extractor_version: u32,
    pub accounting_version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestGuard {
    pub request_id: String,
    pub target: IndexTarget,
    pub work_generation: u64,
}

/// Recheck all fields in the write transaction for success AND failure commits:
/// lease epoch/expiry, revision, generation, Pending, source_present, versions,
/// and active nonterminal request target. Reject instead of applying stale work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexCommitGuard {
    pub lease: IndexLease,
    pub target: IndexTarget,
    pub work_generation: u64,
    pub expected_state: IndexState,
    pub source_present: bool,
    pub request: Option<RequestGuard>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitRejection {
    LeaseLost,
    SourceChanged,
    GenerationChanged,
    StateChanged,
    SourceDeleted,
    VersionChanged,
    RequestInactive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexCommitResult {
    Committed,
    Discarded(CommitRejection),
}

/// Shared extraction signature; measurement must not infer missing provenance.
pub type ExtractIndexInput = for<'a> fn(
    IndexInput<'a>,
    &secret_guard::SecretRedactor,
) -> Result<ExtractedIndexInput, ExtractionError>;

/// W1 repository boundary. Save redacts then atomically persists snapshot,
/// binding, sidecar, source_revision and R2 state. Receipt is visible ONLY after
/// commit. Rollback leaves the old bundle intact; runtime installs nothing.
pub trait SnapshotRepository {
    fn save_snapshot_bundle(
        &mut self,
        write: &SnapshotWrite,
    ) -> Result<SnapshotReceipt, SnapshotApiError>;

    /// Snapshot, sidecar, revisions and latest note must share a read transaction.
    fn load_snapshot_bundle(
        &self,
        run_id: &str,
    ) -> Result<Option<SnapshotBundle>, SnapshotApiError>;

    /// Append (including empty clear) and advance source_revision/R2 state in
    /// one transaction; do not change snapshot_revision.
    fn append_indexed_note(
        &mut self,
        run_id: &str,
        body: &str,
        created_at_ns: i64,
    ) -> Result<NoteReceipt, SnapshotApiError>;

    /// Authorization is the caller's prerequisite, not granted by a locator.
    /// Validate revision/position against source, NEVER return FTS text.
    fn resolve_guarded_reference(
        &self,
        reference: &GuardedReference,
    ) -> Result<GuardedResolution, SnapshotApiError>;
}

/// W1 transaction adapter for evidence/reference-consumption writes. Staging
/// participates in the caller's existing transaction; caller publishes the
/// returned receipt and installs prepared context ONLY after that commit.
pub trait SnapshotTransaction {
    fn stage_snapshot_bundle(
        &mut self,
        write: &SnapshotWrite,
    ) -> Result<SnapshotReceipt, SnapshotApiError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteReceipt {
    pub note: RunLedgerEntry,
    pub source_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardedResolution {
    Message {
        bundle: Box<SnapshotBundle>,
        ordinal: usize,
        block_index: usize,
    },
    /// Latest seq only; empty or superseded notes resolve to StaleReference.
    WorkingNote(RunLedgerEntry),
    StaleReference,
    SourceDeleted,
    UnsupportedVersion,
}

#[derive(Debug, PartialEq)]
pub enum SnapshotApiError {
    Storage(StorageError),
    RevisionConflict,
    InvalidSnapshot,
    InvalidProvenance(ExtractionError),
}

/// W3 maps strict user configuration into this storage-facing value. W2 applies
/// changes at writer-command ordering, not at arbitrary job/read boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThreadIndexSettings {
    pub enabled: bool,
    pub max_bytes: u64,
}

impl Default for ThreadIndexSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_bytes: 512 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexSettingsReceipt {
    pub applied_generation: u64,
    pub settings: ThreadIndexSettings,
}

/// Components are physical upper bounds (including rewritten pages/WAL frames),
/// never estimated from logical eviction credit. Unknown bound rejects admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlannedGrowth {
    Bounded { db: u64, wal: u64, shm: u64 },
    Unbounded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexPauseReason {
    Disabled,
    ReserveInsufficient,
    WalLimit,
    GrowthUnbounded,
    ReclaimExhausted,
    WalReaderPinned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapacityState {
    Normal,
    IndexPaused(IndexPauseReason),
    SourceSuspended(LimitKind),
}

/// Stop at P+G+R >= H or WAL+G_wal >= max_wal; resume only with +J strictly
/// below BOTH limits and source writes resumed. IndexPaused does not imply
/// source suspension. R/J/step bounds require measurements before job work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexCapacityStatus {
    pub state: CapacityState,
    pub db_bytes: u64,
    pub wal_bytes: u64,
    pub shm_bytes: u64,
    pub reserve_bytes: u64,
    pub resume_hysteresis_bytes: u64,
}

pub trait IndexCapacity {
    fn capacity_status(
        &mut self,
        growth: PlannedGrowth,
    ) -> Result<IndexCapacityStatus, StorageError>;
    fn apply_index_settings(
        &mut self,
        settings: ThreadIndexSettings,
    ) -> Result<IndexSettingsReceipt, StorageError>;
}

/// W6 chooses coefficients, not the extractor. One shared accounting API must
/// charge text, attributes, block/index overhead and FTS with checked arithmetic.
/// This is logical accounting, NOT a physical-growth estimator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountingPolicy {
    pub version: u32,
    pub per_block_bytes: u64,
    pub per_run_bytes: u64,
    pub fts_numerator: u64,
    pub fts_denominator: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexAccounting {
    pub accounting_version: u32,
    pub text_bytes: u64,
    pub attribute_bytes: u64,
    pub block_count: u64,
    /// text + attributes + per_block*count + per_run
    /// + ceil(text*fts_numerator/fts_denominator), with no intermediate overflow.
    pub charged_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountingError {
    InvalidPolicy,
    Overflow,
}

pub trait IndexAccountant {
    fn account_index_input(
        &self,
        input: &ExtractedIndexInput,
        policy: AccountingPolicy,
    ) -> Result<IndexAccounting, AccountingError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexOwnerKind {
    Gui,
    Cli,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexOwner {
    pub id: String,
    pub kind: IndexOwnerKind,
}

/// DB-scoped fencing token. Expired owners cannot commit even before takeover.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexLease {
    pub owner: IndexOwner,
    pub epoch: u64,
    pub expires_at_ns: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseResult {
    Acquired(IndexLease),
    HeldByOther(IndexLease),
    Lost,
}

/// canonical project equality; since is inclusive source_updated_at_ns >= since.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReindexRequest {
    pub canonical_project: Option<String>,
    pub since_ns: Option<i64>,
    pub include_evicted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", content = "detail", rename_all = "snake_case")]
pub enum RequestGapReason {
    Blocked(IndexBlockedReason),
    CapacityPaused,
    Oversized,
    EvictedByBudget,
    Deleted,
    Superseded,
    VersionSuperseded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestGapCount {
    pub reason: RequestGapReason,
    pub count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestSummary {
    pub target_count: u64,
    pub succeeded: u64,
    pub deleted_before_submission: u64,
    pub gaps: Vec<RequestGapCount>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestFailure {
    InvalidArguments,
    DatabaseUnavailable,
    PersistenceUnavailable,
    UnrecoverableControlFailure,
}

/// Accepted is ONLY a non-wait response delegated to another live owner.
/// All-gap is CompletedWithGaps, never Failed. Terminal results are immutable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ReindexResult {
    Accepted {
        request_id: String,
    },
    Completed {
        request_id: String,
        summary: RequestSummary,
    },
    CompletedWithGaps {
        request_id: String,
        summary: RequestSummary,
    },
    Failed {
        request_id: Option<String>,
        reason: RequestFailure,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestProgress {
    Pending(RequestSummary),
    Terminal(ReindexResult),
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestStatus {
    pub request_id: String,
    pub progress: RequestProgress,
    /// None diagnoses a persisted request waiting for the next GUI/CLI owner.
    pub owner: Option<IndexLease>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestSubmission {
    pub request_id: String,
    /// Submission and owner election are fenced atomically. If acquired, CLI
    /// must foreground-step to terminal even WITHOUT --wait (no hidden daemon).
    pub lease: LeaseResult,
    pub progress: RequestProgress,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexStepResult {
    Progress,
    Idle { next_wake_at_ns: Option<i64> },
    Paused(IndexCapacityStatus),
    LeaseLost,
}

/// Shared GUI/CLI control boundary; no CLI-only mutation path. Clock is explicit.
/// Submission materializes immutable revision/version targets transactionally.
/// Successful/failed work uses IndexCommitGuard; cancellation invalidates just
/// that request, not work shared by other requests. Finalization rechecks retained
/// revisions/versions (including already processed targets) before terminalizing.
pub trait ThreadIndexControl {
    fn submit_request(
        &mut self,
        request: &ReindexRequest,
        owner: &IndexOwner,
        now_ns: i64,
        lease_duration_ns: i64,
    ) -> Result<RequestSubmission, RequestFailure>;
    fn request_status(
        &self,
        request_id: &str,
        now_ns: i64,
    ) -> Result<RequestStatus, RequestFailure>;
    fn cancel_request(&mut self, request_id: &str) -> Result<RequestStatus, RequestFailure>;
    fn acquire_lease(
        &mut self,
        owner: &IndexOwner,
        now_ns: i64,
        lease_duration_ns: i64,
    ) -> Result<LeaseResult, RequestFailure>;
    fn heartbeat(
        &mut self,
        lease: &IndexLease,
        now_ns: i64,
        lease_duration_ns: i64,
    ) -> Result<LeaseResult, RequestFailure>;
    fn release_lease(&mut self, lease: &IndexLease) -> Result<(), RequestFailure>;
    fn step(&mut self, lease: &IndexLease, now_ns: i64) -> Result<IndexStepResult, RequestFailure>;
}
