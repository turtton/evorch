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
    ApprovalPending,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestartBlocker {
    pub run: crate::ids::RunId,
    pub kind: RestartBlockerKind,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RestartCancelReason {
    ApprovalPending { pending: u64 },
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel_reason: Option<RestartCancelReason>,
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
            cancel_reason: None,
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
    #[test]
    fn cancel_reason_is_absent_on_the_wire_until_a_restart_was_cancelled_for_one() {
        let mut status = RestartStatus {
            restart_id: None,
            phase: RestartPhase::Running,
            revision: 3,
            blockers: vec![RestartBlocker {
                run: crate::ids::RunId::new("1").unwrap(),
                kind: RestartBlockerKind::ApprovalPending,
            }],
            blocker_total: 1,
            blocker_omitted: 0,
            active_producers: 0,
            cancel_reason: None,
        };
        let wire = serde_json::to_value(&status).unwrap();
        assert!(wire.get("cancel_reason").is_none());
        assert_eq!(wire["blockers"][0]["kind"], "approval_pending");
        status.cancel_reason = Some(RestartCancelReason::ApprovalPending { pending: 2 });
        let wire = serde_json::to_value(&status).unwrap();
        assert_eq!(
            wire["cancel_reason"],
            serde_json::json!({"kind": "approval_pending", "pending": 2})
        );
        assert_eq!(
            serde_json::from_value::<RestartStatus>(wire).unwrap(),
            status
        );
    }
}
