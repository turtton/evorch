//! Lanes namespace adapter: lane management, always as a human action.
use workbench_protocol::commands::CommandReply;
use workbench_protocol::envelope::Provenance;
use workbench_protocol::errors::WorkbenchError;
use workbench_protocol::namespaces::{LanesCommand, LanesQuery, LanesReply};
use workbench_protocol::queries::QueryReply;

use crate::ingress::CommandHost;

impl CommandHost {
    pub(in crate::ingress) fn lanes_command(
        &mut self,
        command: LanesCommand,
        provenance: &Provenance,
    ) -> Result<CommandReply, WorkbenchError> {
        match command {
            LanesCommand::ManageLane(manage) => self.manage_lane_command(manage, provenance),
        }
    }

    pub(in crate::ingress) fn lanes_query(
        &self,
        query: &LanesQuery,
    ) -> Result<QueryReply, WorkbenchError> {
        match query {
            LanesQuery::GetLaneManagement { lane_id } => Ok(QueryReply::Lanes(
                LanesReply::LaneManagement(self.lane_management_state(lane_id.as_str())?),
            )),
        }
    }
}
