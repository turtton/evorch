//! Workspace namespace adapter: listings and details of projects and threads are
//! read from the projection; everything the shared sidebar owns (trust, allowed
//! directories, families, renames) is done by `self.areas.workspace`.
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
        provenance: &Provenance,
    ) -> Result<CommandReply, WorkbenchError> {
        self.areas.workspace.command(command, provenance)
    }

    pub(in crate::ingress) fn workspace_query(
        &self,
        query: &Query,
        namespace_query: &WorkspaceQuery,
    ) -> Result<QueryReply, WorkbenchError> {
        match namespace_query {
            WorkspaceQuery::ListProjects(_)
            | WorkspaceQuery::ListThreads(_)
            | WorkspaceQuery::GetThread(_) => self.projection.query(query),
            WorkspaceQuery::GetProjectAccess(_)
            | WorkspaceQuery::ListFamilies(_)
            | WorkspaceQuery::GetThreadDetail(_)
            | WorkspaceQuery::GetRetention(_) => self
                .areas
                .workspace
                .query(namespace_query)
                .map(QueryReply::Workspace),
        }
    }
}
