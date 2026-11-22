//! Host admission capability shared with the runtime capture boundary.
pub use runtime::quiesce::{
    CommandAccess, HostCommandPermit, HostInvocationLease, HostRestartGate, HostRestartPhase,
    InvocationBlock, SealedRestart,
};
use runtime::{RunId, RuntimeError, StopScope};

/// Core installs the single lifecycle coordinator; embedded hosts retain runtime Stop.
pub trait RestartStopHandler: std::any::Any + Send + Sync {
    fn stop(&self, run: RunId, scope: StopScope) -> Result<(), RuntimeError>;
}

#[derive(Clone, Default)]
pub struct PreparingServices {
    pub evaluations: eval::evaluator::EvaluationService,
}
