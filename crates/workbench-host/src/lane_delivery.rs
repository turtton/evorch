//! The host's adapter for the runtime's durable lane delivery (ADR 0044 Phase 1c).
//!
//! The runtime owns the outbox, claim lease, receipts, wake guard and Stop; it asks this host
//! only *who* a thread currently is: its current root run and the authority (ownership permit,
//! project, model) under which a finished root may be restored. A sidebar name is never used to
//! guess a root, and a thread this host cannot act for answers with a typed NACK so the
//! notifications stay durable and the lease is released.
//!
//! The host runs on the runtime's executor, not the UI thread, so it reads a snapshot of the
//! command sink's bindings that the sink republishes after every change.

use std::path::PathBuf;

use crate::lane::{BindingTable, TeamSource, memory_boundary};
use config::agent_categories::CategoryId;
use runtime::lane_delivery::{LaneDeliveryHost, LaneDeliveryTarget};
use runtime::{Role, RunConfig, WeakAgentRuntime};
use storage::lane_delivery::LaneDeliveryNack;

/// Resolves lane delivery destinations from the host's trusted thread/project bindings.
/// It holds the runtime weakly: the runtime owns this host.
pub struct HostLaneDeliveryHost {
    runtime: WeakAgentRuntime,
    bindings: BindingTable,
    memory_config: Option<storage::StorageConfig>,
    team_writer: Option<storage::StorageHandle>,
    sidebar_path: Option<PathBuf>,
}

impl HostLaneDeliveryHost {
    pub(crate) fn new(
        runtime: WeakAgentRuntime,
        bindings: BindingTable,
        memory_config: Option<storage::StorageConfig>,
        team_writer: Option<storage::StorageHandle>,
        sidebar_path: Option<PathBuf>,
    ) -> Self {
        Self {
            runtime,
            bindings,
            memory_config,
            team_writer,
            sidebar_path,
        }
    }
}

impl LaneDeliveryHost for HostLaneDeliveryHost {
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
        let saved_role = runtime
            .restore_diagnostics(root)
            .ok()
            .flatten()
            .map(|saved| saved.role_name);
        let worker = saved_role.as_deref() == Some(Role::Worker.name());
        let project_root = crate::lane::current_thread_project(
            &runtime,
            self.sidebar_path.as_deref(),
            thread,
            binding
                .as_ref()
                .and_then(|binding| binding.project_root.as_deref()),
        )
        .map_err(|error| match error.code {
            workbench_protocol::errors::ErrorCode::NotFound => LaneDeliveryNack::UnknownThread,
            workbench_protocol::errors::ErrorCode::StorageUnavailable => {
                LaneDeliveryNack::StorageFailed
            }
            _ => LaneDeliveryNack::RestoreFailed,
        })?;
        runtime
            .bind_thread_root(thread, root)
            .map_err(|_| LaneDeliveryNack::RestoreFailed)?;
        let mut authority = RunConfig {
            conversation: worker || saved_role.as_deref() == Some(Role::Orchestrator.name()),
            category: worker.then(|| CategoryId::Conversation.to_string()),
            memory: memory_boundary(
                &runtime,
                self.memory_config.as_ref(),
                project_root.as_deref(),
            ),
            project_root,
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
