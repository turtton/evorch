//! Lifecycle namespace: what the core is doing and why it stays up.
use crate::{
    handshake::{Capability, CoreStatus, WireLimits},
    validation::ValidationError,
};
use serde::{Deserialize, Serialize};

pub const COMMAND_OPERATIONS: &[&str] = &[];
pub const QUERY_OPERATIONS: &[&str] = &["get_core_status"];
pub const EVENT_TYPES: &[&str] = &["core_status_changed"];
/// Tagged choices inside a command's payload that classification names (none).
pub const NESTED_COMMANDS: &[super::NestedOperation] = &[];

no_commands!(LifecycleCommand);
no_results!(LifecycleResult);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum LifecycleQuery {
    GetCoreStatus,
}

impl LifecycleQuery {
    pub fn required_capability(&self) -> Capability {
        Capability::Queries
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
}

impl LifecycleReply {
    pub fn validate(&self, _limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::CoreStatus(status) => status.validate(),
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
    CoreStatusChanged { status: CoreStatus },
}

impl LifecycleEvent {
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::CoreStatusChanged { status } => status.validate(),
        }
    }
}
