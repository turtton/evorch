//! What the lane delivery host reads from the command service (ADR 0044 Phase 1c).
//!
//! The runtime's lane delivery asks a host only who a thread currently is: its root run and the
//! authority a finished root may be restored under. The service republishes a snapshot of its
//! bindings after every change ([`BindingTable`]) because the delivery host runs on the
//! runtime's executor, not on the thread that owns the service.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use runtime::{ModelPreference, RunConfig, RunId};

/// What the command sink knows of a thread's conversation, copied for the host.
#[derive(Clone)]
pub struct ThreadBinding {
    pub root: RunId,
    pub project_root: Option<PathBuf>,
    pub goal_project: Option<String>,
    pub model_preference: Option<ModelPreference>,
}

/// The sink's bindings as of its last change, shared with the host.
#[derive(Clone, Default)]
pub struct BindingTable(
    Arc<Mutex<BTreeMap<String, ThreadBinding>>>,
    Arc<std::sync::atomic::AtomicU64>,
);
/// Opaque non-authority observation. Commit checks it without the host mutex.
#[derive(Clone)]
pub struct BindingObservation {
    generation: Arc<std::sync::atomic::AtomicU64>,
    expected: u64,
    bindings: Arc<Mutex<BTreeMap<String, ThreadBinding>>>,
}
impl BindingObservation {
    /// Hold the publication exclusion through a memory-only admission boundary.
    pub fn with_current<T>(&self, transition: impl FnOnce() -> T) -> Option<T> {
        let _guard = self.bindings.lock().ok()?;
        self.is_current().then(transition)
    }
    pub fn is_current(&self) -> bool {
        self.expected != u64::MAX
            && self.generation.load(std::sync::atomic::Ordering::Acquire) == self.expected
    }
}

impl BindingTable {
    pub fn observe(&self) -> BindingObservation {
        BindingObservation {
            generation: self.1.clone(),
            bindings: self.0.clone(),
            expected: self.1.load(std::sync::atomic::Ordering::Acquire),
        }
    }
    pub fn replace(&self, bindings: BTreeMap<String, ThreadBinding>) {
        let mut current = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Invalidate BEFORE publishing. Exhaustion is permanently unavailable;
        // never wrap or reuse a binding observation after A -> B -> A.
        let _ = self.1.try_update(
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
            |v| v.checked_add(1),
        );
        *current = bindings;
    }

    pub fn get(&self, thread: &str) -> Option<ThreadBinding> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(thread)
            .cloned()
    }
}

/// Reads the lessons the new run starts from; a failure only drops them.
pub fn memory_boundary(
    runtime: &runtime::AgentRuntime,
    config: Option<&storage::StorageConfig>,
    root: Option<&Path>,
) -> Option<runtime::memory::MemoryBoundary> {
    let config = config?;
    let root = root?;
    let project = runtime.project_slug(root).unwrap_or_else(|| {
        runtime::repository::derive_repo_slug_owned(root, runtime.spawn_ledger())
    });
    runtime::memory::MemoryBoundary::capture(config, &project)
        .inspect_err(|error| tracing::warn!(%error, %project, "memory read failed"))
        .ok()
}

/// What renewing a restored team coordinator's current capability reads.
pub struct TeamSource<'a> {
    pub runtime: &'a runtime::AgentRuntime,
    pub memory_config: Option<&'a storage::StorageConfig>,
    pub team_writer: Option<&'a storage::StorageHandle>,
    pub goal_project: Option<&'a str>,
}

impl TeamSource<'_> {
    /// Renews the current team capability of a restored team coordinator. It is not a model
    /// message.
    pub fn renew(
        &self,
        thread: &str,
        run: RunId,
        prompt: Option<&str>,
        authority: &mut RunConfig,
    ) -> Result<(), String> {
        if !self
            .runtime
            .restore_diagnostics(run)
            .ok()
            .flatten()
            .is_some_and(|diagnostics| diagnostics.renewable_team.is_some())
        {
            return Ok(());
        }
        let project = self
            .goal_project
            .ok_or("Current project binding is required to continue this team")?;
        let (Some(config), Some(writer)) = (self.memory_config, self.team_writer) else {
            return Err("Current team storage is unavailable".into());
        };
        authority.team_store = Some(runtime::team_context::TeamStore {
            config: config.clone(),
            writer: writer.clone(),
            id: format!("{project}:{thread}"),
        });
        authority.topology = runtime::CoordinationTopology::DynamicTeam { max_workers: 3 };
        authority.delegation_value = Some(
            prompt
                .unwrap_or("Resume the existing team coordination")
                .into(),
        );
        authority.finding_store = Some(config.db_path.clone());
        Ok(())
    }
}

#[cfg(test)]
mod binding_observation_tests {
    use super::*;
    #[test]
    fn equal_binding_aba_and_exhaustion_never_revalidate_an_observation() {
        let table = BindingTable::default();
        let proof = table.observe();
        assert!(proof.is_current());
        table.replace(Default::default());
        table.replace(Default::default());
        assert!(!proof.is_current());
        assert!(table.observe().is_current());
        table
            .1
            .store(u64::MAX - 1, std::sync::atomic::Ordering::Release);
        let last = table.observe();
        table.replace(Default::default());
        assert!(!last.is_current());
        assert!(!table.observe().is_current());
        table.replace(Default::default());
        assert!(!table.observe().is_current());
    }
}
