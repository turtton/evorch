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

    /// The projection serves the operations listed here. An operation the
    /// workspace area adds later gets its own arm, answered by
    /// `self.areas.workspace`; the compiler asks for it.
    pub(in crate::ingress) fn workspace_query(
        &self,
        query: &Query,
        namespace_query: &WorkspaceQuery,
    ) -> Result<QueryReply, WorkbenchError> {
        match namespace_query {
            WorkspaceQuery::ListProjects(_)
            | WorkspaceQuery::ListThreads(_)
            | WorkspaceQuery::GetThread(_) => self.projection.query(query),
        }
    }
}
