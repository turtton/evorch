//! Diagnostics namespace adapter (read side, PR 13): the namespace names no
//! command; its queries read through the `diagnostics` area.
use workbench_protocol::commands::CommandReply;
use workbench_protocol::envelope::Provenance;
use workbench_protocol::errors::WorkbenchError;
use workbench_protocol::namespaces::diagnostics::{
    DiagnosticsCommand, DiagnosticsQuery, DiagnosticsReply,
};
use workbench_protocol::queries::{Query, QueryReply};

use crate::ingress::CommandHost;

impl CommandHost {
    pub(in crate::ingress) fn diagnostics_command(
        &mut self,
        command: DiagnosticsCommand,
        _provenance: &Provenance,
        _restart: crate::bootstrap::restart::HostCommandPermit,
    ) -> Result<CommandReply, WorkbenchError> {
        match command {}
    }

    pub(in crate::ingress) fn diagnostics_query(
        &self,
        _query: &Query,
        query: &DiagnosticsQuery,
    ) -> Result<QueryReply, WorkbenchError> {
        let diagnostics = &self.areas.diagnostics;
        Ok(QueryReply::Diagnostics(match query {
            DiagnosticsQuery::GetUsageSummary(get) => {
                DiagnosticsReply::Usage(Box::new(diagnostics.usage(get)?))
            }
            DiagnosticsQuery::GetRunContext(get) => {
                DiagnosticsReply::Context(Box::new(diagnostics.context(get)?))
            }
            DiagnosticsQuery::GetDiagnosticsOperation(target) => {
                DiagnosticsReply::Operation(diagnostics.operation(&target.operation_id)?)
            }
        }))
    }
}

impl CommandHost {
    /// How a planned restart treats this namespace's commands. The namespace has
    /// none yet, so this cannot be called; the PR that adds the first command
    /// writes the classification here (default: new work), not in the shared
    /// `restart.rs`.
    pub(in crate::ingress) fn diagnostics_command_access(
        command: &DiagnosticsCommand,
    ) -> crate::bootstrap::restart::CommandAccess {
        match *command {}
    }
}
