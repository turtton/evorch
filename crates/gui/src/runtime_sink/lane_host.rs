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

use std::sync::Arc;

use config::agent_categories::CategoryId;
use runtime::lane_delivery::{LaneDeliveryHost, LaneDeliveryTarget};
use runtime::ownership::{OwnerHost, OwnerPermit, OwnershipError, RegistryError};
use runtime::{Role, RunConfig, WeakAgentRuntime};
use storage::lane_delivery::LaneDeliveryNack;
use workbench_host::lane::{BindingTable, TeamSource, memory_boundary};

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
