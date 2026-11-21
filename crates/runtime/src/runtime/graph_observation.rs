//! Synchronous host observation fences. No execution or disposal authority.
use super::*;

/// Implementations register the fence synchronously, before any memory reservation
/// or durable effect. Readback is called only by the original supervisor, outside
/// all runtime locks. Dropping a ticket does NOT confirm an outcome.
pub trait GraphObserver: Send + Sync + 'static {
    fn begin_change(&self, manifest: &str) -> Result<u64, RuntimeError>;
    /// Observation failure permanently closes management, not automatic recovery.
    fn begin_automatic_change(&self, manifest: &str) -> Result<Option<u64>, RuntimeError>;
    fn cancel_unstarted(&self, manifest: &str, ticket: u64) -> Result<(), RuntimeError>;
    fn readback(&self, manifest: &str, ticket: u64) -> Result<(), RuntimeError>;
    fn rebuild(&self) -> Result<(), RuntimeError>;
    fn mark_unavailable(&self);
}

#[derive(Clone)]
pub(super) struct GraphChange {
    observer: Arc<dyn GraphObserver>,
    manifest: String,
    ticket: u64,
}
impl GraphChange {
    pub(super) fn mark_unavailable(&self) {
        self.observer.mark_unavailable();
    }
    pub(super) fn cancel_unstarted(&self) -> Result<(), RuntimeError> {
        self.observer.cancel_unstarted(&self.manifest, self.ticket)
    }
    pub(super) fn confirm(&self) -> Result<(), RuntimeError> {
        self.observer.readback(&self.manifest, self.ticket)
    }
}
impl AgentRuntime {
    /// Installed exactly once by bootstrap, including while management is off.
    pub fn install_graph_observer(
        &self,
        observer: Arc<dyn GraphObserver>,
    ) -> Result<(), RuntimeError> {
        self.shared
            .graph_observer
            .set(observer)
            .map_err(|_| RuntimeError::RestartBusy)
    }
    pub fn rebuild_graph_observation(&self) -> Result<(), RuntimeError> {
        if let Some(observer) = self.shared.graph_observer.get() {
            observer.rebuild()?;
        }
        Ok(())
    }
    pub(super) fn begin_graph_change(
        &self,
        manifest: &str,
        automatic: bool,
    ) -> Result<Option<GraphChange>, RuntimeError> {
        let Some(observer) = self.shared.graph_observer.get() else {
            return Ok(None);
        };
        let ticket = if automatic {
            observer.begin_automatic_change(manifest)?
        } else {
            Some(observer.begin_change(manifest)?)
        };
        Ok(ticket.map(|ticket| GraphChange {
            observer: observer.clone(),
            manifest: manifest.into(),
            ticket,
        }))
    }
}
