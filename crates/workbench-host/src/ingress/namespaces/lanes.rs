//! Lanes namespace adapter: lane management and the delivery resume, always as
//! a human action, and the lane listing and detail reads.
use workbench_protocol::commands::{CommandDisposition, CommandReply, CommandResult};
use workbench_protocol::envelope::{Actor, Provenance};
use workbench_protocol::errors::{ErrorCode, RetryAdvice, WorkbenchError};
use workbench_protocol::namespaces::{LanesCommand, LanesQuery, LanesReply, LanesResult};
use workbench_protocol::queries::{Query, QueryReply};

use crate::areas::lanes::resume_deliveries;
use crate::ingress::CommandHost;

impl CommandHost {
    pub(in crate::ingress) fn lanes_command(
        &mut self,
        command: LanesCommand,
        provenance: &Provenance,
    ) -> Result<CommandReply, WorkbenchError> {
        match command {
            LanesCommand::ManageLane(manage) => self.manage_lane_command(manage, provenance),
            LanesCommand::CloseLane(close) => self.close_lane_command(close, provenance),
            LanesCommand::ResumeLaneDeliveries(resume) => {
                if provenance.actor != Actor::Human {
                    return Err(WorkbenchError::new(
                        ErrorCode::PermissionDenied,
                        "human_action_required",
                        RetryAdvice::Never,
                    ));
                }
                let resumed = resume_deliveries(
                    self.service().runtime(),
                    self.service().ownership(),
                    &resume,
                )?;
                if let Err(error) = self.projection().refresh() {
                    tracing::warn!(?error.code, "lane projection refresh failed");
                }
                Ok(CommandReply {
                    disposition: CommandDisposition::Accepted,
                    ..CommandReply::with_result(CommandResult::Lanes(
                        LanesResult::DeliveriesResumed(resumed),
                    ))
                })
            }
        }
    }

    pub(in crate::ingress) fn lanes_query(
        &self,
        lanes_query: &LanesQuery,
    ) -> Result<QueryReply, WorkbenchError> {
        match lanes_query {
            LanesQuery::GetLaneManagement { lane_id } => Ok(QueryReply::Lanes(
                LanesReply::LaneManagement(self.lane_management_state(lane_id.as_str())?),
            )),
            LanesQuery::ListLanes(_)
            | LanesQuery::GetLane { .. }
            | LanesQuery::GetLaneClosePreview { .. } => self
                .areas
                .lanes
                .query(&Query::Lanes(lanes_query.clone()), lanes_query),
        }
    }
}
