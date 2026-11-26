//! Bounded online confirmation contracts. No persisted exploration or owner registry.
use super::{BindingInput, ConfirmedFamily, RetentionInput, ThreadRetentionPolicy};

/// Fixed per-candidate limits; the host measurement harness exercises these limits.
#[derive(Debug, Clone, Copy)]
pub struct OnlineBudget {
    pub members: usize,
    pub edges: u64,
    pub tasks: usize,
    pub side_effect_rows: usize,
    pub side_effect_bytes: u64,
}
impl Default for OnlineBudget {
    fn default() -> Self {
        Self {
            members: 64,
            edges: 2_048,
            tasks: 256,
            side_effect_rows: 1_024,
            side_effect_bytes: 1024 * 1024,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnlineClosure {
    pub threads: Vec<String>,
    pub runs: Vec<String>,
}
#[derive(Debug, Clone)]
pub struct OnlineRequest {
    pub seed: String,
    pub expected: OnlineClosure,
    pub input: RetentionInput,
    pub policy: ThreadRetentionPolicy,
    pub binding: BindingInput,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OnlineOutcome {
    Ready(OnlineClosure),
    Hold(&'static str),
    Confirmed(ConfirmedFamily),
}
/// Memory-only callbacks invoked on the writer thread. Acquiring must atomically
/// compare the caller's admission stamps and seal, without waiting or I/O.
pub trait CommitFence: Send + Sync {
    fn try_seal(&self) -> Option<Box<dyn SealedCommit>>;
}
/// Drop unconditionally releases the seal (including rollback and unwinding).
/// publish must only update the shared memory guard; it cannot fail or do I/O.
pub trait SealedCommit: Send {
    fn publish(&mut self, confirmed: &ConfirmedFamily);
}
