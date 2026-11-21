//! One live, process-local collector shared by Q2 and restart admission.
//! Never read storage while holding state; diagnostic SnapshotClock is unrelated.
#[cfg(test)]
#[path = "collector_tests.rs"]
mod tests;
use super::view::{self, GraphFact};
use crate::admin::{self, AdminFailure};
use runtime::{
    AgentRuntime, RuntimeError, WeakAgentRuntime, runtime::graph_observation::GraphObserver,
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use storage::StorageConfig;
use workbench_protocol::{
    errors::ErrorCode,
    namespaces::lifecycle::owned_graphs::*,
    wire::{InstanceEpoch, Revision},
};

pub struct LiveGraphCollector {
    pub(super) state: Mutex<LiveState>,
    runtime: WeakAgentRuntime,
    storage: StorageConfig,
}
pub(super) struct LiveState {
    pub epoch: Option<InstanceEpoch>,
    pub revision: u64,
    pub ready: bool,
    pub failed: bool,
    pub graphs: BTreeMap<String, GraphFact>,
    pub pending: BTreeMap<String, u64>,
    // Only asks the existing coordinator whether this exact attempt returned to
    // Running. Never interprets receipts or invents a second lifecycle state.
    pub restart: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}
impl LiveState {
    pub fn advance(&mut self) -> Result<u64, AdminFailure> {
        let Some(next) = self.revision.checked_add(1) else {
            self.failed = true;
            return Err(unavailable());
        };
        self.revision = next;
        Ok(next)
    }
    pub fn available(&self) -> Result<(), AdminFailure> {
        if !self.ready || self.failed || self.epoch.is_none() {
            Err(unavailable())
        } else {
            Ok(())
        }
    }
    pub fn reconcile_restart(&mut self) {
        if self.restart.as_ref().is_some_and(|returned| returned()) {
            self.restart = None;
        }
    }
}
pub(super) fn unavailable() -> AdminFailure {
    admin::failure(
        ErrorCode::StorageUnavailable,
        "live graph observation unavailable",
    )
}
fn runtime_error(_: AdminFailure) -> RuntimeError {
    RuntimeError::RestartBusy
}
impl LiveGraphCollector {
    pub fn new(runtime: &AgentRuntime, storage: StorageConfig) -> Arc<Self> {
        Arc::new(Self {
            runtime: runtime.downgrade(),
            storage,
            state: Mutex::new(LiveState {
                epoch: None,
                revision: 0,
                ready: false,
                failed: false,
                graphs: BTreeMap::new(),
                pending: BTreeMap::new(),
                restart: None,
            }),
        })
    }
    /// Stop observing before successor owner release; never reopen management.
    pub fn invalidate(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.failed = true;
        let _ = state.advance();
    }
    pub fn bind_epoch(&self, epoch: InstanceEpoch) -> Result<(), AdminFailure> {
        let mut state = self.state.lock().map_err(|_| unavailable())?;
        if state.epoch.is_some() || epoch.0 == 0 {
            return Err(admin::conflict());
        }
        state.advance()?;
        state.epoch = Some(epoch);
        Ok(())
    }
    pub fn snapshot(&self) -> Result<UndisposedGraphs, AdminFailure> {
        let state = self.state.lock().map_err(|_| unavailable())?;
        state.available()?;
        // Pending/Unknown must not be presented as a successful empty snapshot.
        if !state.pending.is_empty() {
            return Err(unavailable());
        }
        view::aggregate_facts(state.graphs.values(), Revision(state.revision))
    }
    fn facts(&self, manifest: Option<&str>) -> Result<BTreeMap<String, GraphFact>, AdminFailure> {
        let runtime = self.runtime.upgrade().ok_or_else(unavailable)?;
        let graphs =
            storage::Database::read_owned_graphs(&self.storage, |db| db.owned_graph_snapshots())
                .map_err(|_| unavailable())?;
        let mut facts = BTreeMap::new();
        for graph in graphs
            .iter()
            .filter(|g| manifest.is_none_or(|m| m == g.cas.manifest.manifest_id))
        {
            let diagnostic = runtime
                .owned_graph_ownership_readback(&graph.cas)
                .map_err(|_| unavailable())?;
            // Only the sealed adopted inventory can acquire live tickets. Cold
            // and foreign database rows never extend it through a later claim.
            if diagnostic.verified_adoption() {
                let key = graph.cas.manifest.manifest_id.clone();
                if facts
                    .insert(key, view::graph_fact(graph, &diagnostic)?)
                    .is_some()
                {
                    return Err(unavailable());
                }
            }
        }
        Ok(facts)
    }
}
impl GraphObserver for LiveGraphCollector {
    fn mark_unavailable(&self) {
        self.invalidate();
    }
    fn diagnostic_revision(&self, manifest: &str) -> Option<u64> {
        let mut state = self.state.lock().ok()?;
        state.available().ok()?;
        state.reconcile_restart();
        if state.restart.is_some() || state.pending.contains_key(manifest) {
            return None;
        }
        Some(state.revision)
    }
    fn publish_diagnostic(
        &self,
        expected: &storage::recovery::graph_control::GraphControlCas,
        revision: u64,
        diagnostic: &runtime::OwnedGraphOwnershipDiagnostic,
    ) -> Result<(), RuntimeError> {
        let manifest = &expected.manifest.manifest_id;
        // A diagnostic must never overwrite a newer effect result. Check both
        // before I/O and at publication; effect begin advances the revision.
        let current = |state: &LiveState| {
            state.available().is_ok()
                && state.revision == revision
                && state.restart.is_none()
                && !state.pending.contains_key(manifest)
        };
        {
            let state = self.state.lock().map_err(|_| RuntimeError::RestartBusy)?;
            if !current(&state) {
                return Err(RuntimeError::RestartBusy);
            }
            // Cold/foreign rows are diagnostic-only. They still use the same
            // optimistic revision and restart fence, but never grow the sealed
            // live inventory or acquire effect authority through a read.
            if !state.graphs.contains_key(manifest) {
                return Ok(());
            }
        }
        // Project the very ownership result returned to the diagnostic caller,
        // not a second owner read that could disagree with that response.
        let fact =
            storage::Database::read_owned_graphs(&self.storage, |db| db.owned_graph_snapshots())
                .map_err(|_| unavailable())
                .and_then(|graphs| {
                    let graph = graphs
                        .iter()
                        .find(|graph| graph.cas == *expected)
                        .ok_or_else(unavailable)?;
                    if !diagnostic.verified_adoption() {
                        return Err(unavailable());
                    }
                    view::graph_fact(graph, diagnostic)
                });
        let mut state = self.state.lock().map_err(|_| RuntimeError::RestartBusy)?;
        if !current(&state) {
            return Err(RuntimeError::RestartBusy);
        }
        let fact = match fact {
            Ok(fact) => fact,
            Err(error) => {
                state.failed = true;
                let _ = state.advance();
                return Err(runtime_error(error));
            }
        };
        if state.graphs.get(manifest) != Some(&fact) {
            state.advance().map_err(runtime_error)?;
            state.graphs.insert(manifest.into(), fact);
        }
        Ok(())
    }
    fn begin_change(&self, manifest: &str) -> Result<u64, RuntimeError> {
        let mut state = self.state.lock().map_err(|_| RuntimeError::RestartBusy)?;
        state.available().map_err(runtime_error)?;
        state.reconcile_restart();
        if state.restart.is_some()
            || !state.graphs.contains_key(manifest)
            || state.pending.contains_key(manifest)
            || state.pending.len() >= 64
        {
            return Err(RuntimeError::RestartBusy);
        }
        let ticket = state.advance().map_err(runtime_error)?;
        state.pending.insert(manifest.into(), ticket);
        Ok(ticket)
    }
    fn begin_automatic_change(&self, manifest: &str) -> Result<Option<u64>, RuntimeError> {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poison) => {
                let mut state = poison.into_inner();
                state.failed = true;
                state
            }
        };
        state.reconcile_restart();
        if state.restart.is_some() {
            return Err(RuntimeError::RestartBusy);
        }
        if state.failed {
            return Ok(None);
        }
        if state.pending.contains_key(manifest) {
            return Err(RuntimeError::ActivationReservationUnavailable {
                reason: runtime::ActivationReservationProblem::Contended,
            });
        }
        if state.available().is_err()
            || !state.graphs.contains_key(manifest)
            || state.pending.len() >= 64
        {
            state.failed = true;
            return Ok(None);
        }
        let ticket = match state.advance() {
            Ok(ticket) => ticket,
            Err(_) => return Ok(None),
        };
        state.pending.insert(manifest.into(), ticket);
        Ok(Some(ticket))
    }
    fn cancel_unstarted(&self, manifest: &str, ticket: u64) -> Result<(), RuntimeError> {
        let mut state = self.state.lock().map_err(|_| RuntimeError::RestartBusy)?;
        if state.pending.get(manifest) != Some(&ticket) {
            return Err(RuntimeError::RestartBusy);
        }
        state.advance().map_err(runtime_error)?;
        state.pending.remove(manifest);
        Ok(())
    }
    fn readback(&self, manifest: &str, ticket: u64) -> Result<(), RuntimeError> {
        {
            let state = self.state.lock().map_err(|_| RuntimeError::RestartBusy)?;
            match state.pending.get(manifest) {
                None => return Ok(()),
                Some(current) if *current != ticket => return Err(RuntimeError::RestartBusy),
                _ => {}
            }
        }
        let mut facts = match self.facts(Some(manifest)) {
            Ok(facts) => facts,
            Err(error) => {
                self.invalidate();
                return Err(runtime_error(error));
            }
        };
        let Some(fact) = facts.remove(manifest) else {
            self.invalidate();
            return Err(RuntimeError::RestartBusy);
        };
        let mut state = self.state.lock().map_err(|_| RuntimeError::RestartBusy)?;
        if state.pending.get(manifest) != Some(&ticket) {
            return Err(RuntimeError::RestartBusy);
        }
        state.advance().map_err(runtime_error)?;
        state.graphs.insert(manifest.into(), fact);
        state.pending.remove(manifest);
        Ok(())
    }
    fn rebuild(&self) -> Result<(), RuntimeError> {
        // Bootstrap calls this before any automatic attempt. A failed initial
        // read leaves Rebuilding; a later query cannot turn it into Ready.
        let facts = match self.facts(None) {
            Ok(facts) => facts,
            Err(_) => {
                self.invalidate();
                return Ok(());
            }
        };
        let mut state = self.state.lock().map_err(|_| RuntimeError::RestartBusy)?;
        if state.ready || !state.pending.is_empty() {
            return Err(RuntimeError::RestartBusy);
        }
        state.advance().map_err(runtime_error)?;
        state.graphs = facts;
        state.ready = true;
        Ok(())
    }
}
