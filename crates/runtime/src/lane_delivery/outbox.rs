//! The two source adapters share the runtime flight, pending slot and lifecycle.
use super::*;
use storage::{StorageError, StorageHandle, pull_request_auto_fix::PrDeliveryClaim};
#[derive(Clone)]
pub(crate) enum DeliveryClaim {
    Lane(LaneDeliveryClaim),
    PullRequest(PrDeliveryClaim),
}
macro_rules! field {
    ($self:ident, $field:ident) => {
        match $self {
            Self::Lane(c) => &c.$field,
            Self::PullRequest(c) => &c.$field,
        }
    };
}
impl DeliveryClaim {
    pub fn lane(&self) -> Option<&LaneDeliveryClaim> {
        if let Self::Lane(c) = self {
            Some(c)
        } else {
            None
        }
    }
    pub fn owner(&self) -> &LaneDeliveryOwner {
        field!(self, owner)
    }
    pub fn token(&self) -> &str {
        field!(self, token)
    }
    pub fn lease_until_ns(&self) -> i64 {
        *field!(self, lease_until_ns)
    }
    pub fn completed_root_wake(&self) -> bool {
        *field!(self, completed_root_wake)
    }
    pub fn len(&self) -> usize {
        match self {
            Self::Lane(c) => c.notifications.len(),
            Self::PullRequest(c) => c.notifications.len(),
        }
    }
    pub fn received_ids(&self, store: &StorageHandle) -> Result<Vec<String>, StorageError> {
        match self {
            Self::Lane(c) => store.lane_delivery_received_ids(c.clone()),
            Self::PullRequest(c) => store.pr_delivery_receipt_ids(c.clone()),
        }
    }
    pub fn ack(&self, store: &StorageHandle, now: i64) -> Result<(), StorageError> {
        match self {
            Self::Lane(c) => store.ack_lane_delivery(c.clone(), now),
            Self::PullRequest(c) => store.ack_pr_delivery(c.clone(), now),
        }
    }
    pub fn nack(
        &self,
        store: &StorageHandle,
        reason: LaneDeliveryNack,
        now: i64,
    ) -> Result<(), StorageError> {
        match self {
            Self::Lane(c) => store.nack_lane_delivery(c.clone(), reason, now),
            Self::PullRequest(c) => store.nack_pr_delivery(c.clone(), reason, now),
        }
    }
    pub fn check_nudge(
        &self,
        runtime: &AgentRuntime,
        target: &LaneDeliveryTarget,
        flush: bool,
    ) -> Result<(), LaneDeliveryNack> {
        match self {
            Self::Lane(c) => runtime.check_nudge_target(target, c, flush),
            Self::PullRequest(_) => Ok(()),
        }
    }
}
