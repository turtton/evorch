//! The GUI's thin adapter for the runtime's durable lane delivery (ADR 0044 Phase 1c).
//!
//! The runtime owns the outbox, claim lease, receipts, wake guard and Stop; it asks this host
//! only *who* a thread currently is: its current root run and the authority (ownership permit,
//! project, model) under which a finished root may be restored. A sidebar name is never used to
//! guess a root, and a thread this window cannot act for answers with a typed NACK so the
//! notifications stay durable and the lease is released.
//!
//! The host runs on the runtime's executor, not the UI thread, so it reads a snapshot of the
//! command sink's bindings that the sink republishes after every change.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use config::agent_categories::CategoryId;
use runtime::lane_delivery::{LaneDeliveryHost, LaneDeliveryTarget};
use runtime::ownership::{OwnerHost, OwnerPermit, OwnershipError, RegistryError};
use runtime::{ModelPreference, Role, RunConfig, RunId, WeakAgentRuntime};
use storage::lane_delivery::LaneDeliveryNack;

/// What the command sink knows of a thread's conversation, copied for the host.
#[derive(Clone)]
pub(super) struct ThreadBinding {
    pub root: RunId,
    pub project_root: Option<PathBuf>,
    pub goal_project: Option<String>,
    pub model_preference: Option<ModelPreference>,
}

/// The sink's bindings as of its last change, shared with the host.
#[derive(Clone, Default)]
pub(super) struct BindingTable(Arc<Mutex<BTreeMap<String, ThreadBinding>>>);

impl BindingTable {
    pub(super) fn replace(&self, bindings: BTreeMap<String, ThreadBinding>) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = bindings;
    }

    fn get(&self, thread: &str) -> Option<ThreadBinding> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(thread)
            .cloned()
    }
}

/// Reads the lessons the new run starts from; a failure only drops them.
pub(super) fn memory_boundary(
    runtime: &runtime::AgentRuntime,
    config: Option<&storage::StorageConfig>,
    root: Option<&Path>,
) -> Option<runtime::memory::MemoryBoundary> {
    let config = config?;
    let root = root?;
    let project = runtime
        .project_slug(root)
        .unwrap_or_else(|| super::derive_repo_slug(root));
    runtime::memory::MemoryBoundary::capture(config, &project)
        .inspect_err(|error| tracing::warn!(%error, %project, "memory read failed"))
        .ok()
}

/// What renewing a restored team coordinator's current capability reads.
pub(super) struct TeamSource<'a> {
    pub runtime: &'a runtime::AgentRuntime,
    pub memory_config: Option<&'a storage::StorageConfig>,
    pub team_writer: Option<&'a storage::StorageHandle>,
    pub goal_project: Option<&'a str>,
}

impl TeamSource<'_> {
    /// Renews the current team capability of a restored team coordinator. It is not a model
    /// message.
    pub(super) fn renew(
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

/// The ownership permit this window holds for `thread`, or why it holds none.
fn permit_for(host: &OwnerHost, thread: &str) -> Result<OwnerPermit, LaneDeliveryNack> {
    let missing = |error: &RegistryError| {
        matches!(
            error,
            RegistryError::Absent | RegistryError::ThreadDeleted(_)
        )
    };
    match host.owned_permit(thread) {
        Ok(permit) => Ok(permit),
        Err(error) if missing(&error) => Err(LaneDeliveryNack::UnknownThread),
        Err(RegistryError::Ownership(OwnershipError::Fenced)) => match host.attach(thread) {
            Ok(owner) if owner.lease.owner_id != host.owner_id() => Err(LaneDeliveryNack::Readonly),
            Ok(_) => Err(LaneDeliveryNack::OwnershipLost),
            Err(error) if missing(&error) => Err(LaneDeliveryNack::UnknownThread),
            Err(_) => Err(LaneDeliveryNack::OwnershipLost),
        },
        Err(error) if error.sqlite_code().is_some() => Err(LaneDeliveryNack::StorageFailed),
        Err(_) => Err(LaneDeliveryNack::OwnershipLost),
    }
}

/// Resolves lane delivery destinations from the window's current bindings and ownership.
/// It holds the runtime weakly: the runtime owns this host.
pub struct GuiLaneDeliveryHost {
    runtime: WeakAgentRuntime,
    ownership: Option<Arc<OwnerHost>>,
    bindings: BindingTable,
    memory_config: Option<storage::StorageConfig>,
    team_writer: Option<storage::StorageHandle>,
}

impl GuiLaneDeliveryHost {
    pub(super) fn new(
        runtime: WeakAgentRuntime,
        ownership: Option<Arc<OwnerHost>>,
        bindings: BindingTable,
        memory_config: Option<storage::StorageConfig>,
        team_writer: Option<storage::StorageHandle>,
    ) -> Self {
        Self {
            runtime,
            ownership,
            bindings,
            memory_config,
            team_writer,
        }
    }
}

impl LaneDeliveryHost for GuiLaneDeliveryHost {
    fn resolve(&self, thread: &str) -> Result<LaneDeliveryTarget, LaneDeliveryNack> {
        let runtime = self
            .runtime
            .upgrade()
            .ok_or(LaneDeliveryNack::UnknownThread)?;
        let binding = self.bindings.get(thread);
        // A lane thread's root and generation are the lane ledger's: it is the only record that
        // moves with every start or restart, and a delivery to a stale pair is refused.
        let lane = runtime
            .lane_thread_binding(thread)
            .map_err(|_| LaneDeliveryNack::StorageFailed)?;
        let (root, generation) = match (&lane, &binding) {
            (Some((root, generation)), _) => (*root, *generation),
            (None, Some(binding)) => (binding.root, 0),
            // After a restart the sink binds a root only once the user touches the thread; the
            // runtime's saved conversation root is the same answer the Continue path uses.
            (None, None) => (
                runtime
                    .latest_chat_run(thread)
                    .map_err(|_| LaneDeliveryNack::StorageFailed)?
                    .ok_or(LaneDeliveryNack::UnknownThread)?,
                0,
            ),
        };
        let permit = self
            .ownership
            .as_deref()
            .map(|host| permit_for(host, thread))
            .transpose()?;
        let saved_role = runtime
            .restore_diagnostics(root)
            .ok()
            .flatten()
            .map(|saved| saved.role_name);
        let worker = saved_role.as_deref() == Some(Role::Worker.name());
        let project_root = binding
            .as_ref()
            .and_then(|binding| binding.project_root.clone());
        let mut authority = RunConfig {
            conversation: worker || saved_role.as_deref() == Some(Role::Orchestrator.name()),
            category: worker.then(|| CategoryId::Conversation.to_string()),
            ownership: permit,
            memory: memory_boundary(
                &runtime,
                self.memory_config.as_ref(),
                project_root.as_deref(),
            ),
            project_root,
            model_preference: binding
                .as_ref()
                .and_then(|binding| binding.model_preference.clone()),
            ..RunConfig::default()
        };
        TeamSource {
            runtime: &runtime,
            memory_config: self.memory_config.as_ref(),
            team_writer: self.team_writer.as_ref(),
            goal_project: binding
                .as_ref()
                .and_then(|binding| binding.goal_project.as_deref()),
        }
        .renew(thread, root, None, &mut authority)
        .map_err(|_| LaneDeliveryNack::RestoreFailed)?;
        Ok(LaneDeliveryTarget {
            root_run_id: root,
            generation,
            authority,
        })
    }
}
