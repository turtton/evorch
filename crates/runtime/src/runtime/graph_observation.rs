//! Synchronous host observation fences. No execution or disposal authority.
use super::*;
use storage::recovery::graph_control::GraphControlCas;

/// Implementations register the fence synchronously, before any memory reservation
/// or durable effect. Readback is called only by the original supervisor, outside
/// all runtime locks. Dropping a ticket does NOT confirm an outcome.
pub trait GraphObserver: Send + Sync + 'static {
    fn begin_change(&self, manifest: &str) -> Result<u64, RuntimeError>;
    /// Read-only diagnostics do not reserve an effect or advance the aggregate.
    /// Untracked graphs also receive a revision: publication validates the same
    /// snapshot without enrolling them. None means unavailable or contended.
    fn diagnostic_revision(&self, manifest: &str) -> Option<u64>;
    fn publish_diagnostic(
        &self,
        expected: &GraphControlCas,
        revision: u64,
        diagnostic: &OwnedGraphOwnershipDiagnostic,
    ) -> Result<(), RuntimeError>;
    /// Observation failure permanently closes management, not automatic recovery.
    fn begin_automatic_change(&self, manifest: &str) -> Result<Option<u64>, RuntimeError>;
    fn cancel_unstarted(&self, manifest: &str, ticket: u64) -> Result<(), RuntimeError>;
    fn readback(&self, manifest: &str, ticket: u64) -> Result<(), RuntimeError>;
    fn rebuild(&self) -> Result<(), RuntimeError>;
    fn mark_unavailable(&self);
}

pub(super) struct GraphDiagnostic {
    observer: Arc<dyn GraphObserver>,
    manifest: String,
    revision: u64,
}
impl GraphDiagnostic {
    pub(super) fn mark_unavailable(&self) {
        self.observer.mark_unavailable();
    }
    pub(super) fn confirm(
        &self,
        expected: &GraphControlCas,
        diagnostic: &OwnedGraphOwnershipDiagnostic,
    ) -> Result<(), RuntimeError> {
        if expected.manifest.manifest_id != self.manifest {
            return Err(RuntimeError::RestartBusy);
        }
        self.observer
            .publish_diagnostic(expected, self.revision, diagnostic)
    }
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
    pub(super) fn begin_graph_diagnostic(
        &self,
        manifest: &str,
    ) -> Result<Option<GraphDiagnostic>, RuntimeError> {
        let Some(observer) = self.shared.graph_observer.get() else {
            return Ok(None);
        };
        Ok(Some(GraphDiagnostic {
            revision: observer
                .diagnostic_revision(manifest)
                .ok_or(RuntimeError::RestartBusy)?,
            observer: observer.clone(),
            manifest: manifest.into(),
        }))
    }
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
