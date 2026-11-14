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

/// Independent DTOs only: no operation or capability advertises restart yet.
pub const MAX_RESTART_BLOCKERS: usize = 64;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestartPhase {
    Running,
    Requested,
    WaitingSafePoint,
    Sealing,
    Sealed,
    Cancelling,
    FailedClosed,
    ShuttingDown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestartBlockerKind {
    InFlightAdmission,
    AcceptedResumePending,
    AwaitingSafePoint,
    ShellRunning,
    ShellResultUnobserved,
    UnconfirmedRunJs,
    UnfinishedToolBatch,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestartBlocker {
    pub run: crate::ids::RunId,
    pub kind: RestartBlockerKind,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestartStatus {
    pub restart_id: Option<crate::ids::RestartId>,
    pub phase: RestartPhase,
    pub revision: u64,
    pub blockers: Vec<RestartBlocker>,
    pub blocker_total: u64,
    pub blocker_omitted: u64,
    pub active_producers: u64,
}
impl RestartStatus {
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.blockers.len() > MAX_RESTART_BLOCKERS {
            return Err(ValidationError::TooMany {
                field: "restart.blockers",
                max: MAX_RESTART_BLOCKERS,
                actual: self.blockers.len(),
            });
        }
        if self.blocker_total.checked_sub(self.blockers.len() as u64) != Some(self.blocker_omitted)
        {
            return Err(ValidationError::Inconsistent {
                field: "restart.blocker_total",
                reason: "total must include displayed and omitted blockers",
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod restart_tests {
    use super::*;
    #[test]
    fn restart_status_rejects_count_mismatch_and_oversized_blocker_list() {
        let mut status = RestartStatus {
            restart_id: None,
            phase: RestartPhase::WaitingSafePoint,
            revision: 0,
            blockers: vec![],
            blocker_total: 1,
            blocker_omitted: 0,
            active_producers: 0,
        };
        assert!(status.validate().is_err());
        status.blocker_omitted = 1;
        status.validate().unwrap();
        status.blockers = (0..=MAX_RESTART_BLOCKERS)
            .map(|_| RestartBlocker {
                run: crate::ids::RunId::new("1").unwrap(),
                kind: RestartBlockerKind::ShellRunning,
            })
            .collect();
        status.blocker_total = status.blockers.len() as u64;
        status.blocker_omitted = 0;
        assert!(status.validate().is_err());
    }
}
