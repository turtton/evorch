//! Host-owned checks, separate from source incarnation validity.
use super::*;
use crate::bootstrap::restart::{HostRestartGate, HostRestartPhase};
use std::path::{Path, PathBuf};
type TargetCheck = dyn Fn(&ProjectId, &ThreadId) -> bool + Send + Sync;
type ScopeCheck = dyn Fn(&ObservationScope) -> Result<(), ObservationState> + Send + Sync;
#[derive(Clone)]
pub(super) struct Authority {
    pub target: Arc<TargetCheck>,
    pub scope: Arc<ScopeCheck>,
}
impl Authority {
    #[cfg(any(test, feature = "test-support"))]
    pub fn fake() -> Self {
        Self {
            target: Arc::new(|_, _| true),
            scope: Arc::new(|_| Ok(())),
        }
    }
    pub fn from_handles(h: &AreaHandles) -> Self {
        Self::runtime(
            h.runtime.downgrade(),
            h.config_dir.join("sidebar.json"),
            h.restart_gate.clone(),
        )
    }
    pub fn runtime(
        runtime: runtime::WeakAgentRuntime,
        sidebar: PathBuf,
        gate: HostRestartGate,
    ) -> Self {
        let target_runtime = runtime.clone();
        let target_gate = gate.clone();
        let target: Arc<TargetCheck> = Arc::new(move |project, thread| {
            if target_gate.phase() != HostRestartPhase::Open {
                return false;
            }
            let Some(runtime) = target_runtime.upgrade() else {
                return false;
            };
            let Ok(document) = workspace_state::SidebarStore::new(&sidebar).read_unlocked() else {
                return false;
            };
            let Some(project) = document
                .sidebar
                .projects
                .iter()
                .find(|p| p.id.to_string() == project.as_str())
            else {
                return false;
            };
            runtime
                .with_storage_reader(|db| db.thread(thread.as_str()))
                .ok()
                .flatten()
                .flatten()
                .is_some_and(|stored| {
                    stored.project_id.as_deref() == Some(project.id.to_string().as_str())
                        && stored.project_root.as_deref().map(Path::new)
                            == Some(project.repo_root.as_path())
                        && !stored.superseded
                })
        });
        let scope_target = target.clone();
        Self {
            target,
            scope: Arc::new(move |scope| {
                if !scope_target(&scope.project, &scope.thread) {
                    return Err(ObservationState::Revoked);
                }
                let runtime = runtime.upgrade().ok_or(ObservationState::Revoked)?;
                let run: runtime::RunId = scope
                    .run
                    .as_str()
                    .parse()
                    .map_err(|_| ObservationState::Revoked)?;
                let affiliated = runtime
                    .with_storage_reader(|db| db.thread_of_run(scope.run.as_str()))
                    .ok()
                    .flatten()
                    .flatten()
                    .is_some_and(|link| link.thread_id == scope.thread.as_str());
                if !affiliated {
                    return Err(ObservationState::Revoked);
                }
                let reader = runtime.browser_execution_reader();
                let execution = reader.observe(run).ok_or(ObservationState::Revoked)?;
                if execution.incarnation != scope.incarnation.as_str() || !execution.current() {
                    return Err(ObservationState::Revoked);
                }
                let phase = runtime
                    .inspect_agent(run)
                    .map_err(|_| ObservationState::Revoked)?
                    .phase;
                if runtime.graceful_stop_requested(run)
                    || matches!(
                        phase,
                        event_bus::AgentRunPhase::Done
                            | event_bus::AgentRunPhase::Error
                            | event_bus::AgentRunPhase::Stopped
                    )
                {
                    return Err(ObservationState::Stopped);
                }
                // No durable resume intent can establish this live execution.
                if gate.phase() != HostRestartPhase::Open || !execution.current() {
                    return Err(ObservationState::Revoked);
                }
                Ok(())
            }),
        }
    }
    pub fn target(&self, project: &ProjectId, thread: &ThreadId) -> bool {
        (self.target)(project, thread)
    }
    pub fn check(&self, scope: &ObservationScope) -> Result<(), ObservationState> {
        (self.scope)(scope)
    }
}
/// Each lease rechecks host authority and the exact control connection epoch,
/// not just a source token. No operations are added to the shared port contract.
pub(super) struct LeasePort {
    pub port: Arc<dyn AgentObservationPort>,
    pub authority: Authority,
    pub connections: Arc<ConnectionHooks>,
    pub client: ClientId,
    pub epoch: ConnectionEpoch,
}
impl AgentObservationPort for LeasePort {
    fn list(
        &self,
        _: &ProjectId,
        _: &ThreadId,
    ) -> Result<Vec<ObservationScope>, ObservationFailure> {
        Err(ObservationFailure::Revoked)
    }
    fn snapshot(&self, s: &ObservationScope) -> Result<Arc<ObservedPage>, ObservationFailure> {
        if !self.current(s) {
            return Err(ObservationFailure::Revoked);
        }
        self.port.snapshot(s)
    }
    fn subscribe(&self, _: &ObservationScope) -> Result<ObservationFeed, ObservationFailure> {
        Err(ObservationFailure::Revoked)
    }
    fn current(&self, s: &ObservationScope) -> bool {
        self.connections.epoch_of(&self.client) == Some(self.epoch)
            && self.authority.check(s).is_ok()
            && self.port.current(s)
    }
}
