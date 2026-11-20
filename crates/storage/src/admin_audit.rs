//! Durable administrative audit (5a-I): the journal of irreversible operations and the
//! aggregated record of refusals.
//!
//! Storage knows nothing of authority or protocol; the host maps its own types onto the
//! ones here. The module records no credential, token, prompt or tool body: every field
//! is a bounded one-line identifier or a digest, checked on the way in and again on the
//! way out of the database. Time always comes from the caller as milliseconds.
//!
//! Writes go through the single writer ([`crate::StorageHandle::try_submit_admin_audit`]
//! never waits; [`crate::StorageHandle::admin_audit`] is the synchronous form for tests
//! and tools). A write is acknowledged only after its transaction committed, so an
//! acknowledgement that never arrives is resolved by reading the same operation ID back
//! ([`AuditWrite::Read`]), which the writer serves after every earlier write.

use serde::{Deserialize, Serialize};

use crate::{Database, StorageError};

pub(crate) mod denials;
#[cfg(feature = "admin-audit-fault")]
pub mod fault;
pub(crate) mod reconcile;
pub(crate) mod store;
#[cfg(test)]
mod tests;
pub(crate) mod validate;

/// One record, accepted or aggregated, never exceeds this many bytes, reserve included.
pub const AUDIT_MAX_RECORD_BYTES: u64 = 4 * 1024;
/// Rows of irreversible operations kept.
pub const AUDIT_MAX_ACCEPTED_ROWS: u64 = 10_000;
/// Budgeted bytes of irreversible operations kept.
pub const AUDIT_MAX_ACCEPTED_BYTES: u64 = 32 * 1024 * 1024;
/// A finished operation is removed this long after it finished at the latest.
pub const AUDIT_COMPLETED_RETENTION_MS: u64 = 30 * 24 * 60 * 60 * 1000;
/// Rows one cleanup, eviction or reconcile step handles.
pub const AUDIT_BATCH_ROWS: usize = 64;
/// Length of a denial aggregation window.
pub const DENIAL_WINDOW_MS: u64 = 60_000;
/// Distinct caller keys per window; later ones merge into [`DenialKey::Overflow`].
pub const DENIAL_MAX_KEYS_PER_WINDOW: usize = 256;
/// Aggregates kept.
pub const DENIAL_MAX_ROWS: u64 = 1_000;
/// Budgeted bytes of aggregates kept.
pub const DENIAL_MAX_BYTES: u64 = 4 * 1024 * 1024;
/// An aggregate is removed this long after its window started at the latest.
pub const DENIAL_RETENTION_MS: u64 = 24 * 60 * 60 * 1000;

/// The irreversible or state-changing administrative operations that are journaled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditOperation {
    RestartRequest,
    Cancel,
    Interrupt,
    Discard,
    Release,
    Resume,
}

/// Where the request came from, as the host established it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditSurface {
    Desktop,
    Cli,
    Web,
    Unknown,
}

/// Who asked. Never a credential: the method names how the caller was verified, the
/// subject names whom, the binding names the connection or lease it was bound to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditPrincipal {
    /// At most 32 bytes.
    pub method: String,
    /// At most 256 bytes.
    pub subject: String,
    /// At most 128 bytes.
    pub binding: String,
}

impl AuditPrincipal {
    /// The fixed principal of a denial whose caller was not identified.
    #[must_use]
    pub fn anonymous() -> Self {
        Self {
            method: "anonymous".into(),
            subject: "anonymous".into(),
            binding: "anonymous".into(),
        }
    }
}

/// An operation ID with the attempt that holds the right to execute it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditOperationKey {
    /// 1 to 128 bytes, one line.
    pub operation_id: String,
    /// A UUID v4 in its lowercase canonical form (36 bytes), made by the host and never
    /// sent over the wire.
    pub attempt_nonce: String,
}

/// The facts fixed when an operation is accepted. Immutable afterwards.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditAccepted {
    pub key: AuditOperationKey,
    pub operation: AuditOperation,
    pub surface: AuditSurface,
    pub principal: AuditPrincipal,
    pub accepted_core_epoch: u64,
    /// Lowercase SHA-256 hex (64 bytes) of the canonical full intent.
    pub intent_digest: String,
    pub accepted_at_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditState {
    Prepared,
    Started,
    Finished,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditOutcome {
    Succeeded,
    Failed,
    Cancelled,
    Conflict,
    Unknown,
    AbortedBeforeEffect,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditRecord {
    pub accepted: AuditAccepted,
    pub state: AuditState,
    /// At most 128 bytes. Only a restart request has one.
    pub restart_id: Option<String>,
    pub outcome: Option<AuditOutcome>,
    pub finished_at_ms: Option<u64>,
}

/// The answer to [`AuditWrite::Accept`]. `Existing` carries the row of an earlier accept
/// of the same operation ID with the same facts; whether its attempt nonce is the
/// caller's is for the caller to compare: another nonce holds no right to execute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditReservation {
    Created(AuditRecord),
    Existing(AuditRecord),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DenialOperation {
    Targets,
    RestartRequest,
    Cancel,
    Status,
    Interrupt,
    Quarantine,
    Discard,
    Release,
    Resume,
    OwnedGraphs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DenialReason {
    Unauthenticated,
    NonHuman,
    SurfaceMismatch,
    Expired,
    StaleBinding,
    Unavailable,
    Capacity,
}

/// The bounded key refusals are aggregated by. It has no field for a client, session or
/// request ID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum DenialKey {
    Caller {
        /// `None` is stored as [`AuditPrincipal::anonymous`].
        principal: Option<AuditPrincipal>,
        surface: AuditSurface,
        operation: DenialOperation,
        reason: DenialReason,
    },
    Overflow,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DenialAggregate {
    pub key: DenialKey,
    /// A multiple of [`DENIAL_WINDOW_MS`].
    pub window_start_ms: u64,
    /// Inside the window, `first_ms <= last_ms`.
    pub first_ms: u64,
    pub last_ms: u64,
    /// At least 1; saturates at `u64::MAX`.
    pub count: u64,
}

/// What a reconcile step looks at; see [`AuditWrite::Reconcile`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditReconcile {
    /// A cold start: every unfinished operation accepted by another core epoch.
    Cold { live_core_epoch: u64 },
    /// A restart that recorded Ready: the unfinished operations bound to it.
    Ready { restart_id: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuditStats {
    pub accepted_count: u64,
    pub accepted_bytes: u64,
    /// Rows that are not finished.
    pub pending_count: u64,
    pub denial_count: u64,
    pub denial_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuditCleanup {
    pub accepted_removed: u64,
    pub denials_removed: u64,
    pub reconciled: u64,
    /// The step stopped at its batch bound with more to do.
    pub more: bool,
}

/// One command of the journal. All of them run in the order the writer received them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditWrite {
    /// Reserves the operation ID. See [`AuditReservation`].
    Accept(AuditAccepted),
    /// Prepared to Started for the key's attempt. Started again is the same answer.
    Start {
        key: AuditOperationKey,
    },
    /// Reads the row of an operation ID, after every earlier command.
    Read {
        operation_id: String,
    },
    /// Links a restart operation to the restart transaction it asked for. Several
    /// operations may bind one restart; one operation binds one restart only.
    BindRestart {
        key: AuditOperationKey,
        restart_id: String,
    },
    /// Ends the operation. The same outcome again is the same answer; another is a
    /// conflict.
    Finalize {
        key: AuditOperationKey,
        outcome: AuditOutcome,
        at_ms: u64,
    },
    /// Merges up to [`AUDIT_BATCH_ROWS`] aggregates in one transaction.
    Denials {
        rows: Vec<DenialAggregate>,
        now_ms: u64,
    },
    /// Removes expired rows, at most [`AUDIT_BATCH_ROWS`] of each kind.
    Cleanup {
        now_ms: u64,
    },
    /// Ends unfinished operations from what the restart transactions say, at most
    /// [`AUDIT_BATCH_ROWS`] a step.
    Reconcile {
        mode: AuditReconcile,
        now_ms: u64,
    },
    Stats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditReply {
    Reservation(AuditReservation),
    Record(Option<AuditRecord>),
    Applied(AuditRecord),
    /// `stored + dropped` is the number of rows submitted.
    Denials {
        stored: usize,
        dropped: usize,
    },
    /// The answer to both `Cleanup` and `Reconcile`.
    Cleanup(AuditCleanup),
    Stats(AuditStats),
}

/// Called once, on the writer thread, after the command's transaction. It must only hand
/// the result over (for example a oneshot send); it must not block. If the writer stops
/// first the callback is dropped uncalled, and the caller treats the outcome as unknown.
pub type AuditReplyCallback = Box<dyn FnOnce(Result<AuditReply, StorageError>) + Send + 'static>;

/// Why an audit command was refused, as a type a caller can tell apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditError {
    /// The input is malformed or beyond a bound.
    Invalid(&'static str),
    /// The command contradicts what is stored.
    Conflict(AuditConflict),
    /// A bound of the store is reached.
    Capacity(AuditCapacity),
    /// The writer queue is full; nothing was queued.
    Busy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditConflict {
    /// The operation ID exists with different facts.
    SemanticMismatch,
    /// The row belongs to another attempt.
    NonceMismatch,
    /// No such operation ID.
    MissingOperation,
    /// The row's state does not allow the command.
    InvalidTransition,
    /// The row is finished with another outcome.
    OutcomeMismatch,
    /// The operation is bound to another restart.
    RestartIdMismatch,
    /// Only a restart request binds a restart.
    NotRestartOperation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditCapacity {
    /// Unfinished operations alone fill the rows or bytes; nothing can be evicted.
    PendingOperations,
    /// The database is at its size limit and takes no writes.
    WritesSuspended,
}

impl std::fmt::Display for AuditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(reason) => write!(f, "invalid audit input: {reason}"),
            Self::Conflict(conflict) => write!(f, "audit conflict: {conflict:?}"),
            Self::Capacity(capacity) => write!(f, "audit capacity: {capacity:?}"),
            Self::Busy => write!(f, "audit writer queue is full"),
        }
    }
}

impl From<AuditError> for StorageError {
    fn from(error: AuditError) -> Self {
        Self::AdminAudit(error)
    }
}

impl StorageError {
    /// The typed audit refusal, if that is what this error is.
    #[must_use]
    pub fn admin_audit(&self) -> Option<AuditError> {
        match self {
            Self::AdminAudit(error) => Some(*error),
            _ => None,
        }
    }
}

impl Database {
    /// Reads one operation from this connection, without the writer. Tests use it as an
    /// independent readback.
    ///
    /// # Errors
    /// An invalid ID, a database failure, or a row that no longer validates.
    pub fn admin_audit_record(
        &self,
        operation_id: &str,
    ) -> Result<Option<AuditRecord>, StorageError> {
        store::read(&self.conn, operation_id)
    }

    /// Row and byte totals of both tables.
    ///
    /// # Errors
    /// A database failure.
    pub fn admin_audit_stats(&self) -> Result<AuditStats, StorageError> {
        store::stats(&self.conn)
    }

    /// Up to `limit` (at most [`DENIAL_MAX_ROWS`]) aggregates, oldest window first.
    ///
    /// # Errors
    /// A database failure or a row that no longer validates.
    pub fn admin_audit_denials(&self, limit: usize) -> Result<Vec<DenialAggregate>, StorageError> {
        denials::list(&self.conn, limit)
    }
}
