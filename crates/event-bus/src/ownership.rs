use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnershipAction {
    Claimed,
    Heartbeat,
    Suspect,
    Stale,
    Claimable,
    Quiescing,
    Released,
    Handoff,
    MutationRejected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnershipEvent {
    pub thread_id: String,
    pub owner_id: String,
    pub generation: u64,
    pub action: OwnershipAction,
}

impl From<OwnershipEvent> for crate::EventKind {
    fn from(event: OwnershipEvent) -> Self {
        Self::Ownership(event)
    }
}
