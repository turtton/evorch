//! Lanes namespace: lane management (ADR 0044). The actor is attached by the
//! core's ingress, never sent by a client.
use crate::{
    handshake::{Capability, WireLimits},
    lane_attention::LaneId,
    lane_management::{LaneManagementResult, LaneManagementState, ManageLane},
    validation::ValidationError,
};
use serde::{Deserialize, Serialize};

pub const COMMAND_OPERATIONS: &[&str] = &["manage_lane"];
/// `manage_lane`'s `request.action.type` values: the wire names of
/// [`LaneManagementAction`](crate::lane_management::LaneManagementAction)'s
/// variants (a test keeps the two in step).
pub const MANAGE_LANE_ACTIONS: &[&str] = &["set_policy", "detach", "reattach"];
/// The tagged choice inside a command's payload that classification names.
pub const NESTED_COMMANDS: &[super::NestedOperation] = &[super::NestedOperation {
    operation: "manage_lane",
    pointer: "/request/action/type",
    known: MANAGE_LANE_ACTIONS,
}];
pub const QUERY_OPERATIONS: &[&str] = &["get_lane_management"];
pub const EVENT_TYPES: &[&str] = &[];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum LanesCommand {
    ManageLane(ManageLane),
}

impl LanesCommand {
    pub fn required_capability(&self) -> Capability {
        Capability::Commands
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::ManageLane(_) => Ok(()),
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
pub enum LanesResult {
    Managed(LaneManagementResult),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum LanesQuery {
    GetLaneManagement { lane_id: LaneId },
}

impl LanesQuery {
    pub fn required_capability(&self) -> Capability {
        Capability::Queries
    }

    pub fn validate(&self, _limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::GetLaneManagement { .. } => Ok(()),
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
pub enum LanesReply {
    LaneManagement(LaneManagementState),
}

impl LanesReply {
    pub fn validate(&self, _limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::LaneManagement(_) => Ok(()),
        }
    }
}

no_events!(LanesEvent);
