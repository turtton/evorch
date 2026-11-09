//! Desktop adapter; the UI holds its shared-sidebar lock across apply and save.
use crate::app::lane_management::{LaneManagementHost, LaneManagementProjection};
use runtime::{
    AgentRuntime,
    lane_management::{LaneAttachGuard, LaneManagementRequest},
};
pub struct GuiLaneManagementHost {
    adapter: workbench_host::lane_management::HumanLaneManagement,
    runtime: AgentRuntime,
}
impl GuiLaneManagementHost {
    pub fn new(runtime: AgentRuntime) -> Self {
        Self {
            adapter: workbench_host::lane_management::HumanLaneManagement::new(
                runtime.clone(),
                None,
            ),
            runtime,
        }
    }
    pub fn with_ownership(mut self, owner: std::sync::Arc<runtime::ownership::OwnerHost>) -> Self {
        self.adapter = workbench_host::lane_management::HumanLaneManagement::new(
            self.runtime.clone(),
            Some(owner),
        );
        self
    }
}
impl LaneManagementHost for GuiLaneManagementHost {
    fn apply(
        &self,
        request: LaneManagementRequest,
        guard: Option<LaneAttachGuard>,
    ) -> Result<LaneManagementProjection, String> {
        let outcome = self
            .adapter
            .apply(request, guard)
            .map_err(|e| e.to_string())?;
        Ok(LaneManagementProjection {
            current_lane: outcome.current_lane,
        })
    }
}
