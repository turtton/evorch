//! Authenticated desktop management ingress. Use the delivery runtime's clone,
//! not a second runtime/RunStore: management and delivery share its restore gate.

use runtime::AgentRuntime;
use runtime::lane_management::{
    LaneAttachGuard, LaneHumanContext, LaneManagementAction, LaneManagementContext,
    LaneManagementRequest,
};

use crate::app::lane_management::{LaneManagementHost, LaneManagementProjection};

pub struct GuiLaneManagementHost {
    runtime: AgentRuntime,
}

impl GuiLaneManagementHost {
    /// Install only at authenticated desktop ingress. Management calls must hold
    /// the shared-sidebar lock through validation and durable projection.
    pub fn new(runtime: AgentRuntime) -> Self {
        Self { runtime }
    }
}

impl LaneManagementHost for GuiLaneManagementHost {
    fn apply(
        &self,
        request: LaneManagementRequest,
        guard: Option<LaneAttachGuard>,
    ) -> Result<LaneManagementProjection, String> {
        let human = LaneHumanContext;
        let outcome = match &request.action {
            LaneManagementAction::SetPolicy { .. } => self
                .runtime
                .set_lane_policy(LaneManagementContext::User(human), request),
            LaneManagementAction::Detach => self
                .runtime
                .detach_lane(LaneManagementContext::User(human), request),
            LaneManagementAction::Reattach { .. } => self.runtime.reattach_lane(
                human,
                request,
                guard.ok_or("Reattachment requires the locked sidebar family guard.")?,
            ),
        }
        .map_err(|error| error.to_string())?;
        // Even a replay's operation snapshot may be obsolete. None still means
        // committed: the host reports repair pending and rereads the ledger.
        // The host alone saves the file and then mirrors/ACKs this exact revision.
        Ok(LaneManagementProjection {
            current_lane: outcome.current_lane,
        })
    }
}
