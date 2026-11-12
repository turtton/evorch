//! Approvals namespace: tool approval decisions, listings and inputs.
use crate::{
    commands::DecideToolApproval,
    handshake::{Capability, WireLimits},
    limits::MIN_BODY_CHUNK_BYTES,
    queries::{ApprovalInputChunk, GetApprovalInput, PendingQuery},
    validation::ValidationError,
    views::{ApprovalView, Page},
};
use serde::{Deserialize, Serialize};

pub const COMMAND_OPERATIONS: &[&str] = &["decide_tool_approval"];
pub const QUERY_OPERATIONS: &[&str] = &["list_approvals", "get_approval_input"];
pub const EVENT_TYPES: &[&str] = &["approval_changed"];
/// Tagged choices inside a command's payload that classification names (none).
pub const NESTED_COMMANDS: &[super::NestedOperation] = &[];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ApprovalsCommand {
    DecideToolApproval(DecideToolApproval),
}

impl ApprovalsCommand {
    pub fn required_capability(&self) -> Capability {
        Capability::Approvals
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::DecideToolApproval(_) => Ok(()),
        }
    }
}

no_results!(ApprovalsResult);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ApprovalsQuery {
    ListApprovals(PendingQuery),
    GetApprovalInput(GetApprovalInput),
}

impl ApprovalsQuery {
    pub fn required_capability(&self) -> Capability {
        Capability::Approvals
    }

    pub fn validate(&self, limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::ListApprovals(q) => q.page.validate(),
            Self::GetApprovalInput(GetApprovalInput { max_bytes, .. }) => {
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
pub enum ApprovalsReply {
    Approvals(Page<ApprovalView>),
    ApprovalInput(ApprovalInputChunk),
}

impl ApprovalsReply {
    pub fn validate(&self, limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::Approvals(page) => page.validate_with(ApprovalView::validate),
            Self::ApprovalInput(chunk) => chunk.validate(limits.max_body_chunk_bytes),
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
pub enum ApprovalsEvent {
    ApprovalChanged { approval: ApprovalView },
}

impl ApprovalsEvent {
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::ApprovalChanged { approval } => approval.validate(),
        }
    }
}
