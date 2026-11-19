//! O-S root: the bootstrap's existing browser, never a second owner. Until the
//! coherent source adapter lands, every production observation is unavailable.
use super::*;
pub struct AgentObservationSource {
    _browser: Arc<browser::agent::AgentBrowser>,
    _runtime: runtime::runtime::BrowserExecutionReader,
    _core_epoch: u64,
    _tasks: crate::areas::AreaTasks,
}
impl AgentObservationSource {
    pub fn new(
        browser: Arc<browser::agent::AgentBrowser>,
        runtime: runtime::runtime::BrowserExecutionReader,
        core_epoch: u64,
        tasks: crate::areas::AreaTasks,
    ) -> Self {
        Self {
            _browser: browser,
            _runtime: runtime,
            _core_epoch: core_epoch,
            _tasks: tasks,
        }
    }
}
impl AgentObservationPort for AgentObservationSource {
    fn list(
        &self,
        _: &ProjectId,
        _: &ThreadId,
    ) -> Result<Vec<ObservationScope>, ObservationFailure> {
        Err(ObservationFailure::Unavailable)
    }
    fn snapshot(&self, _: &ObservationScope) -> Result<Arc<ObservedPage>, ObservationFailure> {
        Err(ObservationFailure::Unavailable)
    }
    fn subscribe(&self, _: &ObservationScope) -> Result<ObservationFeed, ObservationFailure> {
        Err(ObservationFailure::Unavailable)
    }
    fn current(&self, _: &ObservationScope) -> bool {
        false
    }
}
