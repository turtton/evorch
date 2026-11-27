use serde::{Deserialize, Serialize};

mod checkpoint;
pub use checkpoint::{
    CheckpointBoundary, OwnerCheckpoint, RunCheckpoint, SnapshotNoneReason, SnapshotReference,
};
pub(crate) mod authority;
mod observe;
mod permit;
mod planned;
mod retry;
mod state;
pub use observe::{DbOperation, DbPhase, TARGET as DB_TRACE_TARGET};
#[cfg(test)]
pub(crate) use permit::GenerationClockOverride;
pub use permit::OwnerPermit;
pub use planned::{
    AdoptedOwnershipEvidence, CapturedThreadRuns, ExpectedActiveRuns, ExpectedCapturedRuns,
    ExpectedThreadRuns, PlannedHold,
};
pub use retry::RetryPolicy;
#[cfg(test)]
pub(crate) use retry::virtual_time::VirtualClock;
pub use state::{HeartbeatTick, OwnershipError, ThreadOwner};
#[cfg(feature = "recovery-test-support")]
mod heartbeat_observation;
#[cfg(feature = "recovery-test-support")]
pub use heartbeat_observation::HeartbeatObservations;
pub(crate) mod disposal;
pub(crate) mod disposal_support;
mod registry;
pub use disposal_support::DisposalOwnerReceipt;
mod scratch_archive;
pub use registry::{OwnerVerdict, Registry, RegistryError};

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
