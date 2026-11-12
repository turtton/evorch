//! Run checkpoints independent of process ownership (ADR 0045 §8).
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointBoundary {
    BeforeModelRequest,
    ToolsApplied,
    ToolsWaiting,
    TurnEnd,
    Terminal,
    Idle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotNoneReason {
    NoRunStore,
    WriteFailed,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SnapshotReference {
    RunContext {
        #[serde(flatten)]
        receipt: storage::RunContextReceipt,
    },
    None {
        reason: SnapshotNoneReason,
    },
}

impl SnapshotReference {
    pub const fn none(reason: SnapshotNoneReason) -> Self {
        Self::None { reason }
    }

    /// References are advisory. Missing/replaced snapshots never grant authority.
    pub fn available(&self, record: Option<&storage::RunContextRecord>) -> bool {
        match (self, record) {
            (Self::RunContext { receipt }, Some(record)) => receipt.matches(record),
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunCheckpoint {
    pub snapshot: SnapshotReference,
    pub boundary: CheckpointBoundary,
    pub checkpointed_at_ms: u64,
}
