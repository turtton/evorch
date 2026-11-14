//! Explicit in-process test seam; never installed by production bootstrap or environment.
use super::*;
/// Retains the actual terminal relay guard until a test releases its completion barrier.
/// It does not create a new dispatch epoch or bypass required registration.
pub struct TerminalTailLease {
    pub(super) _guard: crate::quiesce::dispatch::DispatchGuard,
}
pub(super) type TerminalProbe = Arc<dyn Fn(RunId, TerminalTailLease) + Send + Sync>;
impl AgentRuntime {
    /// Observe successful terminal guard registration. The probe must only send an event.
    /// It runs with the dispatch guard held: never acquire runtime locks or re-enter runtime.
    /// Keep the opaque lease in the test to delay tail completion without blocking this callback.
    pub fn set_terminal_registered_probe(
        &self,
        probe: impl Fn(RunId, TerminalTailLease) + Send + Sync + 'static,
    ) -> Result<(), RuntimeError> {
        self.shared
            .terminal_probe
            .set(Arc::new(probe))
            .map_err(|_| crate::quiesce::runtime::error("terminal probe already set"))
    }
}
