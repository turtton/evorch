//! Maintenance namespace (ADR 0045, stage 17d): the online maintenance of the
//! event database, run by the core through its one storage writer. A client
//! never opens the database.
//!
//! One task exists: `backfill_events` (convert the legacy `subkind` / `run_id`
//! columns of the events table). The thread-index rebuild (`reindex-threads`) is
//! not here yet: its request registration is one unbounded writer transaction,
//! which would block every other write. Stage 17d-3 bounds that transaction in
//! storage and then adds the task (it must be done before 19a). Offline tools
//! (`evorch maintenance <other>`, `inspect`) stay outside the core and are not
//! part of this namespace.
//!
//! # Operations
//!
//! `start_maintenance` names an [`OperationId`] and a task and is answered at
//! once with the operation's [`MaintenanceView`]; the work runs in bounded
//! batches in the core. Every committed batch publishes
//! [`MaintenanceEvent::MaintenanceProgress`]; the end publishes
//! [`MaintenanceEvent::MaintenanceSettled`] and is readable with
//! `get_maintenance`.
//!
//! * **Idempotent.** The same id with the same request joins the first
//!   operation (nothing runs twice); the same id with another request is a
//!   `conflict`. An operation the core no longer holds (it restarted) answers
//!   `not_found`; its outcome is unknown, so read the progress counters of a
//!   new operation instead of assuming it finished.
//! * **One at a time per task.** A second operation of a task that already has
//!   one running is refused with `busy`, whoever asks; `list_maintenance` names
//!   the running one.
//! * **Partial results are told apart.** [`MaintenanceView::effect`] says how
//!   much of the task the committed batches cover: `none`, `partial` (a failed
//!   or cancelled operation that committed batches before it ended),
//!   `unknown` or `complete`. The counters count the batches the writer
//!   reported committed, exactly. A writer step that fails can have committed
//!   part of its work (the conversion and the attribution are two transactions)
//!   without saying so: the operation then ends `failed` with `unknown`, the
//!   counters keep what was reported, and a client reads the state again
//!   before it treats the work as done or untouched.
//! * **Cancel.** `cancel_maintenance` stops the operation at its next batch
//!   boundary. The batch in flight commits first; the operation then ends
//!   `cancelled(requested)` with what was done. A planned restart or a shutdown
//!   ends it as `cancelled(shutdown)`, and no batch is committed after that.
//!   Cancelling an operation that already ended answers its final view.
//! * **Bounded.** A backfill operation handles at most `max_rows` rows (the
//!   core applies [`DEFAULT_MAX_ROWS`] when none is named); start another
//!   operation to continue, the counters show what is left.
use super::diagnostics::OperationTarget;
use crate::{
    errors::ErrorCode,
    handshake::{Capability, WireLimits},
    ids::OperationId,
    operation::{OperationState, OperationView},
    queries::PageRequest,
    validation::{ValidationError, check_bytes},
    views::Page,
};
use serde::{Deserialize, Serialize};

pub const COMMAND_OPERATIONS: &[&str] = &["start_maintenance", "cancel_maintenance"];
pub const QUERY_OPERATIONS: &[&str] = &["get_maintenance", "list_maintenance"];
pub const EVENT_TYPES: &[&str] = &["maintenance_progress", "maintenance_settled"];

/// The tasks `start_maintenance` names, by wire name.
pub const MAINTENANCE_TASKS: &[&str] = &["backfill_events"];
/// Tagged choices inside a command's payload that classification names.
pub const NESTED_COMMANDS: &[super::NestedOperation] = &[super::NestedOperation {
    operation: "start_maintenance",
    pointer: "/task/type",
    known: MAINTENANCE_TASKS,
}];

/// Largest `batch_rows` of a backfill.
pub const MAX_BATCH_ROWS: u32 = 100_000;
/// Rows per batch when a backfill names none.
pub const DEFAULT_BATCH_ROWS: u32 = 1_000;
/// Largest `max_rows` of one backfill operation.
pub const MAX_ROWS_LIMIT: u64 = 10_000_000;
/// Rows one backfill operation handles when it names no `max_rows`.
pub const DEFAULT_MAX_ROWS: u64 = 1_000_000;
/// Longest [`MaintenanceView::message`], in bytes.
pub const MAX_MESSAGE_BYTES: usize = 512;

/// Convert the legacy columns of the events table.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackfillEvents {
    /// Rows per batch (one writer transaction); `1..=MAX_BATCH_ROWS`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_rows: Option<u32>,
    /// Stop after about this many rows; `1..=MAX_ROWS_LIMIT`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rows: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum MaintenanceTask {
    BackfillEvents(BackfillEvents),
}

impl MaintenanceTask {
    pub fn kind(&self) -> MaintenanceKind {
        match self {
            Self::BackfillEvents(_) => MaintenanceKind::BackfillEvents,
        }
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::BackfillEvents(task) => {
                if let Some(rows) = task.batch_rows
                    && !(1..=MAX_BATCH_ROWS).contains(&rows)
                {
                    return Err(ValidationError::OutOfRange {
                        field: "batch_rows",
                        reason: "must be 1..=100000",
                    });
                }
                if let Some(rows) = task.max_rows
                    && !(1..=MAX_ROWS_LIMIT).contains(&rows)
                {
                    return Err(ValidationError::OutOfRange {
                        field: "max_rows",
                        reason: "must be 1..=10000000",
                    });
                }
                Ok(())
            }
        }
    }
}

/// Which task an operation runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaintenanceKind {
    BackfillEvents,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartMaintenance {
    pub operation_id: OperationId,
    pub task: MaintenanceTask,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelMaintenance {
    /// The operation to cancel (the id `start_maintenance` named).
    pub operation_id: OperationId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum MaintenanceCommand {
    StartMaintenance(StartMaintenance),
    CancelMaintenance(CancelMaintenance),
}

impl MaintenanceCommand {
    pub fn required_capability(&self) -> Capability {
        Capability::Commands
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::StartMaintenance(start) => start.task.validate(),
            Self::CancelMaintenance(_) => Ok(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum MaintenanceResult {
    /// The operation, pending or settled.
    Operation(Box<MaintenanceView>),
}

/// How much of the task the committed batches cover.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaintenanceEffect {
    /// No batch was committed.
    None,
    /// Some batches were committed and the operation is running, failed or was
    /// cancelled: the work is neither untouched nor done.
    Partial,
    /// The operation ended `ready`: it did what it was asked.
    Complete,
    /// A writer step failed and may have committed part of its work, which
    /// the writer did not report. The counters hold only the batches that were
    /// reported; read the state again, and do not take the work as done.
    Unknown,
}

/// How a `ready` operation ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaintenanceOutcome {
    /// All targeted work is done (or the bound of the operation was reached;
    /// the counters tell what is left).
    Completed,
}

/// What the committed batches did so far, by task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum MaintenanceProgress {
    BackfillEvents {
        converted: u64,
        reattributed: u64,
        /// Rows among the two counts above whose payload no longer decodes
        /// (they got a blank `subkind` and no `run_id`).
        undecodable: u64,
        /// Rows still unconverted after the last committed batch; absent
        /// before the first one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        remaining_unconverted: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        remaining_unattributed: Option<u64>,
    },
}

impl MaintenanceProgress {
    pub fn kind(&self) -> MaintenanceKind {
        match self {
            Self::BackfillEvents { .. } => MaintenanceKind::BackfillEvents,
        }
    }
}

/// An operation and how it stands. The state of `operation` carries the
/// failure class; `message` is a bounded, secret-free sentence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaintenanceView {
    pub operation: OperationView,
    pub task: MaintenanceKind,
    /// Batches committed so far.
    pub batches: u32,
    pub progress: MaintenanceProgress,
    pub effect: MaintenanceEffect,
    /// How a `ready` operation ended; absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<MaintenanceOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl MaintenanceView {
    pub fn validate(&self) -> Result<(), ValidationError> {
        self.operation.validate()?;
        if self.progress.kind() != self.task {
            return Err(ValidationError::Inconsistent {
                field: "progress",
                reason: "the progress belongs to another task",
            });
        }
        if let Some(message) = &self.message {
            check_bytes("message", message, MAX_MESSAGE_BYTES, true)?;
        }
        let ready = self.operation.state == OperationState::Ready;
        if self.outcome.is_some() && !ready {
            return Err(ValidationError::Inconsistent {
                field: "outcome",
                reason: "only a ready operation names an outcome",
            });
        }
        if ready != (self.effect == MaintenanceEffect::Complete) {
            return Err(ValidationError::Inconsistent {
                field: "effect",
                reason: "an operation is complete exactly when it is ready",
            });
        }
        if self.batches == 0 && self.effect == MaintenanceEffect::Partial {
            return Err(ValidationError::Inconsistent {
                field: "effect",
                reason: "a partial effect needs a committed batch",
            });
        }
        if self.effect == MaintenanceEffect::Unknown
            && !matches!(self.operation.state, OperationState::Failed { .. })
        {
            return Err(ValidationError::Inconsistent {
                field: "effect",
                reason: "only a failed operation can leave an unknown effect",
            });
        }
        Ok(())
    }

    /// The failure class when the operation failed.
    pub fn failure_code(&self) -> Option<ErrorCode> {
        match self.operation.state {
            OperationState::Failed { code } => Some(code),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum MaintenanceQuery {
    /// One operation, whatever its state.
    GetMaintenance(OperationTarget),
    /// The operations the core holds, newest first.
    ListMaintenance(ListMaintenance),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListMaintenance {
    #[serde(default)]
    pub page: PageRequest,
}

impl MaintenanceQuery {
    pub fn required_capability(&self) -> Capability {
        Capability::Queries
    }

    pub fn validate(&self, _limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::GetMaintenance(_) => Ok(()),
            Self::ListMaintenance(list) => list.page.validate(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum MaintenanceReply {
    Operation(Box<MaintenanceView>),
    List(Page<MaintenanceView>),
}

impl MaintenanceReply {
    pub fn validate(&self, _limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::Operation(view) => view.validate(),
            Self::List(page) => page.validate_with(MaintenanceView::validate),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum MaintenanceEvent {
    /// A batch was committed and the operation goes on.
    MaintenanceProgress { view: Box<MaintenanceView> },
    /// The operation reached its final state.
    MaintenanceSettled { view: Box<MaintenanceView> },
}

impl MaintenanceEvent {
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::MaintenanceProgress { view } => {
                view.validate()?;
                if view.operation.state.is_final() {
                    return Err(ValidationError::Inconsistent {
                        field: "view",
                        reason: "a progress event names a running operation",
                    });
                }
                Ok(())
            }
            Self::MaintenanceSettled { view } => {
                view.validate()?;
                if !view.operation.state.is_final() {
                    return Err(ValidationError::Inconsistent {
                        field: "view",
                        reason: "a settled event names a final operation",
                    });
                }
                Ok(())
            }
        }
    }
}
