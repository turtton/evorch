use crate::validation::ValidationError;
use serde::{Deserialize, Serialize};
/// Independent DTOs only: no operation or capability advertises restart yet.
pub const MAX_RESTART_BLOCKERS: usize = 64;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestartPhase {
    Running,
    Requested,
    PreparingDrains,
    WaitingSafePoint,
    Sealing,
    Sealed,
    Capturing,
    Finalizing,
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
