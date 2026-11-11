//! Workspace namespace adapter: project and thread listings and details are read from the projection. The
//! namespace has no commands, so its command type is empty.
use workbench_protocol::commands::CommandReply;
use workbench_protocol::envelope::Provenance;
use workbench_protocol::errors::WorkbenchError;
use workbench_protocol::namespaces::{WorkspaceCommand, WorkspaceQuery};
use workbench_protocol::queries::{Query, QueryReply};

use crate::ingress::CommandHost;

impl CommandHost {
    pub(in crate::ingress) fn workspace_command(
        &mut self,
        command: WorkspaceCommand,
        _provenance: &Provenance,
    ) -> Result<CommandReply, WorkbenchError> {
        match command {}
    }

    pub(in crate::ingress) fn workspace_query(
        &self,
        query: &Query,
        _namespace_query: &WorkspaceQuery,
    ) -> Result<QueryReply, WorkbenchError> {
        self.projection.query(query)
    }
}
