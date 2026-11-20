//! Lifecycle dispatch, fixed before independent restart/quarantine/owned leaves.
use crate::{
    handshake::{Capability, CoreStatus, WireLimits},
    validation::ValidationError,
};
use serde::{Deserialize, Serialize};
pub mod contracts;
pub mod owned_graphs;
pub mod quarantine;
pub mod restart;
pub use contracts::*;
pub use restart::{
    MAX_RESTART_BLOCKERS, RestartBlocker, RestartBlockerKind, RestartPhase, RestartStatus,
};
pub const COMMAND_OPERATIONS: &[&str] = &["restart", "quarantine", "owned_graphs"];
pub const QUERY_OPERATIONS: &[&str] = &[
    "get_core_status",
    "operation_status",
    "restart",
    "quarantine",
    "owned_graphs",
];
pub const EVENT_TYPES: &[&str] = &["core_status_changed", "admin_changed"];
pub const NESTED_COMMANDS: &[super::NestedOperation] = &[
    super::NestedOperation {
        operation: "restart",
        pointer: "/type",
        known: &["request", "cancel", "interrupt"],
    },
    super::NestedOperation {
        operation: "quarantine",
        pointer: "/type",
        known: &["discard"],
    },
    super::NestedOperation {
        operation: "owned_graphs",
        pointer: "/type",
        known: &["release", "resume"],
    },
];
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum LifecycleCommand {
    Restart(restart::RestartCommand),
    Quarantine(quarantine::QuarantineCommand),
    OwnedGraphs(owned_graphs::OwnedGraphsCommand),
}
impl LifecycleCommand {
    pub fn required_capability(&self) -> Capability {
        Capability::Commands
    }
    pub fn action(&self) -> AdminAction {
        match self {
            Self::Restart(c) => c.action(),
            Self::Quarantine(_) => AdminAction::Discard,
            Self::OwnedGraphs(owned_graphs::OwnedGraphsCommand::Release(_)) => AdminAction::Release,
            Self::OwnedGraphs(owned_graphs::OwnedGraphsCommand::Resume(_)) => AdminAction::Resume,
        }
    }
    pub fn operation_id(&self) -> &AdminId {
        match self {
            Self::Restart(c) => c.operation_id(),
            Self::Quarantine(quarantine::QuarantineCommand::Discard(c))
            | Self::OwnedGraphs(
                owned_graphs::OwnedGraphsCommand::Release(c)
                | owned_graphs::OwnedGraphsCommand::Resume(c),
            ) => &c.binding.operation_id,
        }
    }
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::Restart(c) => c.validate(),
            Self::Quarantine(quarantine::QuarantineCommand::Discard(c))
            | Self::OwnedGraphs(
                owned_graphs::OwnedGraphsCommand::Release(c)
                | owned_graphs::OwnedGraphsCommand::Resume(c),
            ) => c.validate(),
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
pub enum LifecycleResult {
    Accepted(AdminReceipt),
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum LifecycleQuery {
    GetCoreStatus,
    OperationStatus { operation_id: AdminId },
    Restart(restart::RestartQuery),
    Quarantine(quarantine::QuarantineQuery),
    OwnedGraphs(owned_graphs::OwnedGraphsQuery),
}
impl LifecycleQuery {
    pub fn required_capability(&self) -> Capability {
        Capability::Queries
    }
    pub fn action(&self) -> Option<AdminAction> {
        match self {
            Self::GetCoreStatus => None,
            Self::OperationStatus { .. } => Some(AdminAction::Status),
            Self::Restart(q) => Some(q.action()),
            Self::Quarantine(_) => Some(AdminAction::Quarantine),
            Self::OwnedGraphs(_) => Some(AdminAction::Status),
        }
    }
    pub fn validate(&self, _limits: &WireLimits) -> Result<(), ValidationError> {
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum LifecycleReply {
    CoreStatus(CoreStatus),
    OperationStatus(AdminReceipt),
    Restart(restart::RestartReply),
    Quarantine(quarantine::QuarantineReply),
    OwnedGraphs(owned_graphs::OwnedGraphsReply),
}
impl LifecycleReply {
    pub fn validate(&self, _limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::CoreStatus(s) => s.validate(),
            Self::OperationStatus(r) => contracts::bounded(r),
            Self::Restart(r) => r.validate(),
            Self::Quarantine(r) => r.validate(),
            Self::OwnedGraphs(r) => r.validate(),
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
pub enum LifecycleEvent {
    CoreStatusChanged {
        status: CoreStatus,
    },
    AdminChanged {
        revision: crate::wire::Revision,
        restarting: bool,
    },
}
impl LifecycleEvent {
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::CoreStatusChanged { status } => status.validate(),
            Self::AdminChanged { .. } => Ok(()),
        }
    }
}
