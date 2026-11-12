//! Projection namespace: reads of the core's projected state (snapshot pages,
//! message bodies, command receipts) and the notices that control a
//! subscription (resync, close).
use crate::{
    events::{CloseReason, ResyncReason},
    handshake::{Capability, WireLimits},
    ids::RequestId,
    limits::MIN_BODY_CHUNK_BYTES,
    queries::{CommandReceipt, ReadBody, ReadSnapshot, SnapshotPage},
    validation::ValidationError,
    views::BodyChunk,
};
use serde::{Deserialize, Serialize};

pub const COMMAND_OPERATIONS: &[&str] = &[];
pub const QUERY_OPERATIONS: &[&str] = &["get_command_receipt", "read_body", "read_snapshot"];
pub const EVENT_TYPES: &[&str] = &["resync_required", "subscription_closed"];
/// Tagged choices inside a command's payload that classification names (none).
pub const NESTED_COMMANDS: &[super::NestedOperation] = &[];

no_commands!(ProjectionCommand);
no_results!(ProjectionResult);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ProjectionQuery {
    /// What became of an earlier command, looked up by its request id.
    GetCommandReceipt {
        request_id: RequestId,
    },
    ReadBody(ReadBody),
    ReadSnapshot(ReadSnapshot),
}

impl ProjectionQuery {
    pub fn required_capability(&self) -> Capability {
        match self {
            Self::GetCommandReceipt { .. } => Capability::Commands,
            Self::ReadBody(_) => Capability::CommittedBodies,
            Self::ReadSnapshot(_) => Capability::Queries,
        }
    }

    pub fn validate(&self, limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::GetCommandReceipt { .. } => Ok(()),
            Self::ReadSnapshot(q) => q.page.validate(),
            Self::ReadBody(ReadBody { max_bytes, .. }) => {
                if *max_bytes < MIN_BODY_CHUNK_BYTES || *max_bytes > limits.max_body_chunk_bytes {
                    return Err(ValidationError::OutOfRange {
                        field: "max_bytes",
                        reason: "outside the negotiated body chunk range",
                    });
                }
                Ok(())
            }
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
pub enum ProjectionReply {
    CommandReceipt(CommandReceipt),
    Body(BodyChunk),
    Snapshot(SnapshotPage),
}

impl ProjectionReply {
    pub fn validate(&self, limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::CommandReceipt(receipt) => receipt.validate(),
            Self::Body(chunk) => chunk.validate(limits.max_body_chunk_bytes),
            Self::Snapshot(snapshot) => snapshot
                .page
                .validate_with(crate::events::SnapshotItem::validate),
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
pub enum ProjectionEvent {
    ResyncRequired { reason: ResyncReason },
    SubscriptionClosed { reason: CloseReason },
}

impl ProjectionEvent {
    pub fn validate(&self) -> Result<(), ValidationError> {
        Ok(())
    }

    /// Whether the subscription is over after this event.
    pub fn ends_subscription(&self) -> bool {
        true
    }
}
