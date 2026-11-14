//! Mcp namespace adapter: translates the protocol's operations into the `mcp`
//! area service (`self.areas.mcp`). A command only names its project; the
//! directory is resolved here, from the project directory the core owns.
use workbench_protocol::commands::{CommandDisposition, CommandReply, CommandResult};
use workbench_protocol::envelope::Provenance;
use workbench_protocol::errors::WorkbenchError;
use workbench_protocol::namespaces::mcp::{McpCommand, McpQuery, McpReply, McpResult};
use workbench_protocol::queries::{Query, QueryReply};

use crate::areas::mcp::project_of;
use crate::ingress::CommandHost;

impl CommandHost {
    pub(in crate::ingress) fn mcp_command(
        &mut self,
        command: McpCommand,
        _provenance: &Provenance,
        restart: crate::bootstrap::restart::HostCommandPermit,
    ) -> Result<CommandReply, WorkbenchError> {
        let root = project_of(&command)
            .map(|project| self.project_root(project))
            .transpose()?;
        let submitted = self
            .areas
            .mcp
            .submit_tracked(command, root, Some(restart))?;
        let mut reply =
            CommandReply::with_result(CommandResult::Mcp(McpResult::Operation(submitted.view)));
        if submitted.already_applied {
            reply.disposition = CommandDisposition::AlreadyApplied;
        }
        Ok(reply)
    }

    pub(in crate::ingress) fn mcp_query(
        &self,
        _query: &Query,
        query: &McpQuery,
    ) -> Result<QueryReply, WorkbenchError> {
        let mcp = &self.areas.mcp;
        Ok(QueryReply::Mcp(match query {
            McpQuery::GetSettings { project } => {
                self.project_root(project)?;
                McpReply::Settings(Box::new(mcp.settings(project)?))
            }
            McpQuery::GetOperation { operation_id } => {
                McpReply::Operation(mcp.operation(operation_id)?)
            }
            McpQuery::ListOperations { project } => McpReply::Operations {
                operations: mcp.operations(project.as_ref())?,
            },
        }))
    }
}
