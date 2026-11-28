//! One live, process-local collector shared by Q2 and restart admission.
//! Never read storage while holding state; diagnostic SnapshotClock is unrelated.
#[cfg(test)]
#[path = "collector_tests.rs"]
mod tests;
use super::view::{self, GraphFact};
use crate::admin::{self, AdminFailure};
use runtime::{RuntimeError, runtime::graph_observation::GraphObserver};
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

type ObservedGraphs = (
    BTreeMap<String, GraphFact>,
    BTreeMap<String, GraphReference>,
);

pub struct LiveGraphCollector {
    pub(super) state: Mutex<LiveState>,
    storage: StorageConfig,
}
pub(super) struct LiveState {
    pub epoch: Option<InstanceEpoch>,
    pub revision: u64,
    pub ready: bool,
    pub failed: bool,
    pub graphs: BTreeMap<String, GraphFact>,
    /// Graphs in a format this build does not execute, by manifest. Display
    /// only: never part of the live inventory, so no effect can start on them.
    pub legacy: BTreeMap<String, GraphReference>,
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
    pub fn new(storage: StorageConfig) -> Arc<Self> {
        Arc::new(Self {
            storage,
            state: Mutex::new(LiveState {
                epoch: None,
                revision: 0,
                ready: false,
                failed: false,
                graphs: BTreeMap::new(),
                legacy: BTreeMap::new(),
                pending: BTreeMap::new(),
                restart: None,
            }),
        })
    }
    /// Stop observing before retiring this core; never reopen management.
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
        view::aggregate_facts(
            state.graphs.values(),
            state.legacy.values(),
            Revision(state.revision),
        )
    }
    /// This core's epoch, once bound.
    pub fn epoch(&self) -> Option<InstanceEpoch> {
        self.state.lock().ok().and_then(|state| state.epoch)
    }
    /// One read snapshot: the live facts of graphs this core adopted, and the
    /// legacy graphs to show. Only the sealed adopted inventory can acquire
    /// live tickets; cold and legacy database rows never extend it through a
    /// later claim.
    fn observe(&self, manifest: Option<&str>) -> Result<ObservedGraphs, AdminFailure> {
        // Never read storage while holding the state.
        let live_epoch = self.state.lock().map_err(|_| unavailable())?.epoch;
        let (graphs, headers) = storage::Database::read_owned_graphs(&self.storage, |db| {
            Ok((db.owned_graph_snapshots()?, db.recovery_graph_headers()?))
        })
        .map_err(|_| unavailable())?;
        let mut facts = BTreeMap::new();
        for graph in graphs.iter().filter(|g| {
            manifest.is_none_or(|m| m == g.cas.manifest.manifest_id)
                && view::adopted_by(g, live_epoch)
        }) {
            let key = graph.cas.manifest.manifest_id.clone();
            if facts
                .insert(key, view::graph_fact(graph, live_epoch)?)
                .is_some()
            {
                return Err(unavailable());
            }
        }
        let mut legacy = BTreeMap::new();
        for header in headers
            .iter()
            .filter(|h| manifest.is_none_or(|m| m == h.manifest_id) && view::legacy_listed(h))
        {
            legacy.insert(header.manifest_id.clone(), view::legacy_reference(header)?);
        }
        Ok((facts, legacy))
    }
    fn facts(&self, manifest: Option<&str>) -> Result<BTreeMap<String, GraphFact>, AdminFailure> {
        self.observe(manifest).map(|(facts, _)| facts)
    }
    /// Re-read the legacy graphs after an explicit discard. A read failure
    /// closes observation (it never becomes an empty list).
    pub fn refresh_legacy(&self) -> Result<(), AdminFailure> {
        let legacy = match self.observe(None) {
            Ok((_, legacy)) => legacy,
            Err(error) => {
                self.invalidate();
                return Err(error);
            }
        };
        let mut state = self.state.lock().map_err(|_| unavailable())?;
        state.available()?;
        if state.legacy != legacy {
            state.advance()?;
            state.legacy = legacy;
        }
        Ok(())
    }
}
impl GraphObserver for LiveGraphCollector {
    fn mark_unavailable(&self) {
        self.invalidate();
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
        let (facts, legacy) = match self.observe(None) {
            Ok(observed) => observed,
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
        state.legacy = legacy;
        state.ready = true;
        Ok(())
    }
}
