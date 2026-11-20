//! Public restart leaf contract. Product coordinator registration is deferred.
use super::contracts::*;
use crate::{
    validation::ValidationError,
    wire::{InstanceEpoch, Revision},
};
use serde::{Deserialize, Serialize};
mod legacy;
pub use legacy::*;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestartBinding {
    pub target_id: AdminId,
    pub expected_core_epoch: InstanceEpoch,
    pub target_revision: Revision,
    pub operation_id: AdminId,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestRestart {
    pub binding: RestartBinding,
    pub confirmed: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelRestart {
    pub restart_id: AdminId,
    pub expected_core_epoch: InstanceEpoch,
    pub expected_revision: Revision,
    pub operation_id: AdminId,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InterruptBinding {
    pub restart_id: AdminId,
    pub core_epoch: InstanceEpoch,
    pub connection_epoch: InstanceEpoch,
    pub run_incarnation: AdminId,
    pub blocker_identity: AdminId,
    pub revision: Revision,
    pub phase: RestartPhase,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InterruptShell {
    pub binding: InterruptBinding,
    pub operation_id: AdminId,
    pub confirmed: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum RestartCommand {
    Request(RequestRestart),
    Cancel(CancelRestart),
    Interrupt(InterruptShell),
}
impl RestartCommand {
    pub fn action(&self) -> AdminAction {
        match self {
            Self::Request(_) => AdminAction::Request,
            Self::Cancel(_) => AdminAction::Cancel,
            Self::Interrupt(_) => AdminAction::Interrupt,
        }
    }
    pub fn operation_id(&self) -> &AdminId {
        match self {
            Self::Request(r) => &r.binding.operation_id,
            Self::Cancel(r) => &r.operation_id,
            Self::Interrupt(r) => &r.operation_id,
        }
    }
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::Request(r) => confirmation(r.confirmed),
            Self::Interrupt(r) => confirmation(r.confirmed),
            Self::Cancel(_) => Ok(()),
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
pub enum RestartQuery {
    Targets,
    Status,
}
impl RestartQuery {
    pub fn action(&self) -> AdminAction {
        match self {
            Self::Targets => AdminAction::Targets,
            Self::Status => AdminAction::Status,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestartTarget {
    pub id: AdminId,
    pub revision: Revision,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestartTargets {
    pub revision: Revision,
    pub targets: Vec<RestartTarget>,
    pub truncated: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum RestartReply {
    Targets(RestartTargets),
    Status(RestartStatus),
}
impl RestartReply {
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::Targets(t) => items(&t.targets)?,
            Self::Status(s) => s.validate()?,
        };
        bounded(self)
    }
}
