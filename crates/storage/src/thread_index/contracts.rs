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

/// Header-only discovery must not read messages_json. `retention_at_ns` is the
/// run's creation time, not its latest snapshot/note update time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexSourceHeader {
    pub target: IndexTarget,
    pub snapshot_revision: u64,
    pub context_sha256: String,
    pub note_seq: Option<u64>,
    pub provenance_version: Option<u32>,
    pub source_updated_at_ns: i64,
    pub retention_at_ns: i64,
    pub source_present: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexRunStatus {
    pub source: IndexSourceHeader,
    pub state: IndexState,
    pub indexed_revision: Option<u64>,
    pub work_generation: u64,
    pub indexed_at_ns: Option<i64>,
    pub text_bytes: u64,
    pub block_count: u64,
    pub charged_bytes: u64,
    pub priority: i64,
    pub attempt_count: u32,
    pub next_attempt_at_ns: Option<i64>,
    pub blocked_reason: Option<IndexBlockedReason>,
    /// Stable sanitized code only, not a database/parser/tool diagnostic string.
    pub last_error_code: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexFailure {
    Transient,
    Blocked(IndexBlockedReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexEvictionReason {
    LogicalBudget,
    Oversized,
    PhysicalReclaim,
}

/// W1 SQL boundary called only by W2's single writer. Replacement diffs,
/// external-content FTS, totals, state and request targets share ONE transaction.
/// Same hash never skips locator updates. Generation advances on eviction,
/// deletion and Pending transitions; delayed success/failure is discarded.
pub trait ThreadIndexRepository {
    fn discover_source(&mut self, header: &IndexSourceHeader) -> Result<(), StorageError>;
    fn index_run_status(&self, run_id: &str) -> Result<Option<IndexRunStatus>, StorageError>;
    fn commit_index_input(
        &mut self,
        guard: &IndexCommitGuard,
        input: &ExtractedIndexInput,
        accounting: IndexAccounting,
        now_ns: i64,
    ) -> Result<IndexCommitResult, StorageError>;
    fn record_index_failure(
        &mut self,
        guard: &IndexCommitGuard,
        failure: IndexFailure,
        now_ns: i64,
    ) -> Result<IndexCommitResult, StorageError>;
    fn evict_index(
        &mut self,
        run_id: &str,
        reason: IndexEvictionReason,
    ) -> Result<(), StorageError>;
    /// Remove blocks/FTS/totals and leave a permanent tombstone; rediscovery must
    /// not resurrect it. Source removal and this operation share a transaction.
    fn delete_index_source(&mut self, run_id: &str) -> Result<(), StorageError>;
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

impl From<StorageError> for SnapshotApiError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

impl From<rusqlite::Error> for SnapshotApiError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Storage(error.into())
    }
}

/// W3 maps strict user configuration into this storage-facing value. W2 applies
/// changes at writer-command ordering, not at arbitrary job/read boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThreadIndexSettings {
    pub enabled: bool,
    pub max_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Acknowledges writer-ordered settings, not completion of budget/version
/// adjustment. `capacity_status` reports `SettingsAdjustment` until bounded
/// maintenance steps finish; physical pauses retain their specific reason.
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
    SettingsAdjustment,
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
    /// Sum of UTF-8 bytes of run_id (once), each locator's compact serde JSON,
    /// serialized role/kind names (without quotes), tool_name when present, and
    /// hex text hashes. Fixed numeric/row/index costs belong to per-block/run.
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
    /// Index disabled by settings while the request was pending.
    IndexDisabled,
    /// One run alone exceeds `max_bytes` (`evicted(oversized)`).
    OversizedBudget,
    /// One run exceeds the measured single-step write bound (`evicted(oversized)`).
    OversizedStep,
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
#[serde(tag = "status", content = "detail", rename_all = "snake_case")]
pub enum RequestTargetState {
    Pending,
    Succeeded,
    Gap(RequestGapReason),
}

/// Immutable target identity plus its current shared work generation. Generation
/// rebinding affects ALL live requests sharing the work in the same transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestTargetStatus {
    pub request_id: String,
    pub target: IndexTarget,
    pub work_generation: u64,
    pub state: RequestTargetState,
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
