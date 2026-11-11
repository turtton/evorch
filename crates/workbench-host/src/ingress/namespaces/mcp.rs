//! Mcp namespace adapter (reserved). The namespace has no operations yet,
//! so its command and query types are empty and these bodies are unreachable;
//! the PR that implements it replaces them, without editing the shared router.
use workbench_protocol::commands::CommandReply;
use workbench_protocol::envelope::Provenance;
use workbench_protocol::errors::WorkbenchError;
use workbench_protocol::namespaces::mcp::{McpCommand, McpQuery};
use workbench_protocol::queries::{Query, QueryReply};

use crate::ingress::CommandHost;

impl CommandHost {
    pub(in crate::ingress) fn mcp_command(
        &mut self,
        command: McpCommand,
        _provenance: &Provenance,
    ) -> Result<CommandReply, WorkbenchError> {
        match command {}
    }

    pub(in crate::ingress) fn mcp_query(
        &self,
        _query: &Query,
        query: &McpQuery,
    ) -> Result<QueryReply, WorkbenchError> {
        match *query {}
    }
}
