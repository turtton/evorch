//! Teams namespace adapter: the board is read by the teams area. The namespace
//! has no command, so its command type is empty and that arm is unreachable.
use workbench_protocol::commands::CommandReply;
use workbench_protocol::envelope::Provenance;
use workbench_protocol::errors::WorkbenchError;
use workbench_protocol::namespaces::teams::{TeamsCommand, TeamsQuery};
use workbench_protocol::queries::{Query, QueryReply};

use crate::ingress::CommandHost;

impl CommandHost {
    pub(in crate::ingress) fn teams_command(
        &mut self,
        command: TeamsCommand,
        _provenance: &Provenance,
    ) -> Result<CommandReply, WorkbenchError> {
        match command {}
    }

    pub(in crate::ingress) fn teams_query(
        &self,
        query: &Query,
        teams_query: &TeamsQuery,
    ) -> Result<QueryReply, WorkbenchError> {
        self.areas.teams.query(query, teams_query)
    }
}
