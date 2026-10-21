use serde::{Deserialize, Serialize};

mod maintenance;
pub use maintenance::compact_owner_checkpoints;
mod checkpoint;
pub use checkpoint::{
    CheckpointBoundary, OwnerCheckpoint, RunCheckpoint, SnapshotNoneReason, SnapshotReference,
};
mod observe;
mod permit;
mod retry;
mod state;
pub use observe::{DbOperation, DbPhase, TARGET as DB_TRACE_TARGET};
pub use permit::OwnerPermit;
pub use retry::RetryPolicy;
pub use state::{OwnershipError, ThreadOwner};
mod registry;
pub use registry::{Registry, RegistryError};
#[cfg(unix)]
mod host;
#[cfg(unix)]
pub mod ipc;
#[cfg(unix)]
pub use host::OwnerHost;

#[cfg(test)]
mod contention_tests;
#[cfg(test)]
mod tests;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OwnerState {
    Running,
    Suspect,
    Stale,
    Claimable,
    Quiescing,
    Released,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub owner_id: String,
    pub generation: u64,
    pub expires_at: u64,
}

impl Lease {
    pub const fn observe(&self, now_ms: u64, grace_ms: u64) -> OwnerState {
        if now_ms < self.expires_at {
            OwnerState::Running
        } else if now_ms < self.expires_at.saturating_add(grace_ms) {
            OwnerState::Suspect
        } else {
            OwnerState::Stale
        }
    }
}
