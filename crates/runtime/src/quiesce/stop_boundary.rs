//! Synchronous outer lifecycle boundary for every operator Stop, including direct callers.
use crate::{AgentRuntime, RunId, RuntimeError, StopScope};
use std::sync::Arc;

/// Installed once by the core. Called before runtime locks, never from inside dispatch.
/// Implementations must not retain a strong runtime reference (which would form a cycle).
pub trait StopBoundary: Send + Sync {
    fn stop(&self, delivery: StopDelivery<'_>) -> Result<(), RuntimeError>;
}

/// One-use delivery of the original Stop. Consuming it cannot recurse through the boundary.
/// Delivery still enforces runtime's capture/terminal-dispatch gate; this is not a bypass.
pub struct StopDelivery<'a> {
    runtime: &'a AgentRuntime,
    run: RunId,
    scope: StopScope,
    expected_incarnation: Option<String>,
    between_turns_only: bool,
}
impl<'a> StopDelivery<'a> {
    pub(crate) fn new(runtime: &'a AgentRuntime, run: RunId, scope: StopScope) -> Self {
        Self {
            runtime,
            run,
            scope,
            expected_incarnation: None,
            between_turns_only: false,
        }
    }
    /// Deliver only while the run's execution is still this incarnation.
    pub(crate) fn expecting_incarnation(mut self, incarnation: String) -> Self {
        self.expected_incarnation = Some(incarnation);
        self
    }
    pub(crate) fn between_turns_only(mut self) -> Self {
        self.between_turns_only = true;
        self
    }
    /// After the lifecycle owner chooses shutdown, commit terminal-only runtime
    /// admission before Stop. This cannot cancel/replay the pending restart.
    pub fn deliver_for_shutdown(self) -> Result<(), RuntimeError> {
        self.runtime.begin_dispatch_shutdown()?;
        self.deliver()
    }
    pub fn deliver(self) -> Result<(), RuntimeError> {
        self.runtime.with_terminal_dispatch(|| {
            self.runtime.stop_with_dispatch(
                self.run,
                self.scope,
                self.expected_incarnation.as_deref(),
                self.between_turns_only,
            )
        })
    }
}
impl AgentRuntime {
    /// Composition-only: install before publishing the runtime or accepting commands.
    /// The lifecycle owner cannot be replaced during a preparation. Embedded runtimes
    /// that install no boundary keep their existing Stop behavior.
    pub fn install_stop_boundary(
        &self,
        boundary: Arc<dyn StopBoundary>,
    ) -> Result<(), RuntimeError> {
        self.shared
            .stop_boundary
            .set(boundary)
            .map_err(|_| RuntimeError::RestartBusy)
    }
}
