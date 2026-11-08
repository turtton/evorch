//! Host-side lane operations and shared-sidebar membership repair.
#[cfg(test)]
#[path = "lane_management_tests.rs"]
mod tests;
use super::WorkbenchState;
use crate::model::tasks::AgentRunSource;
use workspace_ui::SidebarState;

impl<S: AgentRunSource> WorkbenchState<S> {
    /// Called with the shared file lock held, before ANY file edit, including
    /// pin/archive. SQL membership is authoritative; epoch merge keeps later
    /// user edits in the same independent period.
    pub(super) fn repair_sidebar_membership(
        &self,
        sidebar: &mut SidebarState,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some(link) = &self.thread_mirror else {
            return Ok(());
        };
        let snapshot = link.handle.sidebar_projection_snapshot(
            sidebar.threads.iter().map(|t| t.id.to_string()).collect(),
        )?;
        for lane in &snapshot.lanes {
            sidebar.set_lane_link(
                &workspace_ui::ThreadId::new(&lane.thread_id),
                super::lanes::link_of(lane),
            )?;
        }
        // Discard obsolete membership states before comparing SQL floors. An old
        // epoch may have much higher revisions, but cannot mask the new reset.
        sidebar.refresh_derived();
        sidebar.raise_family_revisions(
            &|id| {
                snapshot
                    .revision_floors
                    .get(&id.to_string())
                    .copied()
                    .unwrap_or_default()
            },
            false,
        );
        sidebar.refresh_derived();
        Ok(())
    }
}

/// Pin/archive intent stays in the membership period the user saw. Concurrent
/// edits of the two independent state series are still merged normally.
pub(super) fn check_family_membership(
    sidebar: &SidebarState,
    id: &workspace_ui::ThreadId,
    expected: &workspace_ui::lane_management::FamilyRevision,
) -> Result<(), workspace_ui::ThreadError> {
    let current = sidebar
        .lane_family_revision(id)
        .map_err(|error| workspace_ui::ThreadError::UnresolvedFamily(error.to_string()))?;
    if current.key != expected.key || current.membership_epoch != expected.membership_epoch {
        return Err(workspace_ui::ThreadError::UnresolvedFamily(
            "Family membership changed; refresh before pinning or archiving.".into(),
        ));
    }
    Ok(())
}

/// A successful SQL commit, even when the post-commit refresh was unavailable.
/// Never substitute the operation receipt's historical lane snapshot here.
pub struct LaneManagementProjection {
    pub current_lane: Option<storage::lanes::LaneRecord>,
}

/// Trusted runtime adapter; neither a model request nor a drag payload grants
/// human authority. The desktop installs this with its authenticated ingress.
pub trait LaneManagementHost: Send + Sync {
    fn apply(
        &self,
        request: storage::lane_management::LaneManagementRequest,
        guard: Option<storage::lane_management::LaneAttachGuard>,
    ) -> Result<LaneManagementProjection, String>;
}

impl<S: AgentRunSource> WorkbenchState<S> {
    #[must_use]
    pub fn with_lane_management_host(
        mut self,
        host: std::sync::Arc<dyn LaneManagementHost>,
    ) -> Self {
        self.lanes.management_host = Some(host);
        self
    }

    pub fn reattach_lane_drop(
        &mut self,
        intent: workspace_ui::lane_management::LaneDropIntent,
    ) -> Result<(), super::WorkbenchError> {
        use storage::lane_management::{LaneManagementAction, LaneManagementRequest};
        let request = LaneManagementRequest {
            operation_id: uuid::Uuid::new_v4().to_string(),
            lane_id: intent.source.lane_id.clone(),
            expected_management_revision: intent.source.expected_management_revision,
            action: LaneManagementAction::Reattach {
                lead_thread_id: intent.target_thread_id.to_string(),
                expected_lead_root_run_id: intent
                    .expected_lead_root_run_id
                    .clone()
                    .unwrap_or_default(),
            },
        };
        self.manage_lane(request, Some(intent))
    }

    /// The linearization point is the runtime's storage commit. Never reparent
    /// optimistically. A file/mirror failure after it is repair-pending success,
    /// not permission to replay a transition with a new revision.
    pub fn manage_lane(
        &mut self,
        request: storage::lane_management::LaneManagementRequest,
        drop: Option<workspace_ui::lane_management::LaneDropIntent>,
    ) -> Result<(), super::WorkbenchError> {
        use storage::lane_management::{LaneAttachGuard, LaneManagementAction};
        let host = self.lanes.management_host.clone().ok_or_else(|| {
            super::WorkbenchError::LaneManagement("Lane management host is unavailable.".into())
        })?;
        let mut applied = None;
        let result = self.shared_sidebar_transact_host(|latest| {
            let guard = if let Some(intent) = &drop {
                latest.sidebar.validate_lane_drop(intent)?;
                if request.lane_id != intent.source.lane_id
                    || request.expected_management_revision
                        != intent.source.expected_management_revision
                    || request.action
                        != (LaneManagementAction::Reattach {
                            lead_thread_id: intent.target_thread_id.to_string(),
                            expected_lead_root_run_id: intent
                                .expected_lead_root_run_id
                                .clone()
                                .unwrap_or_default(),
                        })
                {
                    return Err("Drop intent and management request disagree.".into());
                }
                Some(LaneAttachGuard {
                    pinned: false,
                    archived: false,
                })
            } else {
                if matches!(request.action, LaneManagementAction::Reattach { .. }) {
                    return Err("Reattachment requires a validated human sidebar drop.".into());
                }
                None
            };
            let source_thread = latest
                .sidebar
                .threads
                .iter()
                .find(|thread| {
                    thread
                        .lane
                        .as_ref()
                        .is_some_and(|lane| lane.lane_id == request.lane_id)
                })
                .map(|thread| thread.id.to_string())
                .ok_or("The lane is not in the current sidebar; refresh and try again.")?;
            let projection = host.apply(request, guard).map_err(std::io::Error::other)?;
            applied = Some(source_thread);
            let current = projection
                .current_lane
                .ok_or("Post-commit ledger refresh unavailable.")?;
            latest.sidebar.set_lane_link(
                &workspace_ui::ThreadId::new(&current.thread_id),
                super::lanes::link_of(&current),
            )?;
            Ok(())
        });
        if let Some(thread) = applied {
            self.request_delivery_reads(&thread);
            self.refresh_retention();
            if let Err(error) = result {
                // The ledger will be reread even if the event was lost. Retention
                // and stale mirrors remain fenced until a durable repair + ACK.
                self.save_sidebar();
                return Err(super::WorkbenchError::LaneManagement(format!(
                    "Lane management committed; sidebar repair is pending: {error}"
                )));
            }
            Ok(())
        } else {
            result.map_err(|error| super::WorkbenchError::LaneManagement(error.to_string()))
        }
    }
}
