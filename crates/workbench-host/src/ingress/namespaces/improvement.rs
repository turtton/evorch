//! Improvement namespace adapter (read side, PR 13): the namespace names no
//! command, and a query only names its project; the directory is resolved here,
//! from the project directory the core owns.
use workbench_protocol::commands::CommandReply;
use workbench_protocol::envelope::Provenance;
use workbench_protocol::errors::WorkbenchError;
use workbench_protocol::namespaces::improvement::{
    ImprovementCommand, ImprovementQuery, ImprovementReply,
};
use workbench_protocol::queries::{Query, QueryReply};

use crate::ingress::CommandHost;

impl CommandHost {
    pub(in crate::ingress) fn improvement_command(
        &mut self,
        command: ImprovementCommand,
        _provenance: &Provenance,
        _restart: crate::bootstrap::restart::HostCommandPermit,
    ) -> Result<CommandReply, WorkbenchError> {
        match command {}
    }

    pub(in crate::ingress) fn improvement_query(
        &self,
        _query: &Query,
        query: &ImprovementQuery,
    ) -> Result<QueryReply, WorkbenchError> {
        let improvement = &self.areas.improvement;
        Ok(QueryReply::Improvement(match query {
            ImprovementQuery::ListImprovement(list) => ImprovementReply::List(Box::new(
                improvement.list(self.project_root(&list.project)?, list)?,
            )),
            ImprovementQuery::GetImprovement(get) => ImprovementReply::Detail(Box::new(
                improvement.get(self.project_root(&get.project)?, get)?,
            )),
            ImprovementQuery::GetImprovementOperation(target) => {
                ImprovementReply::Operation(improvement.operation(&target.operation_id)?)
            }
        }))
    }
}

impl CommandHost {
    /// How a planned restart treats this namespace's commands. The namespace has
    /// none yet, so this cannot be called; PR 14, which adds the first command,
    /// writes the classification here (default: new work), not in the shared
    /// `restart.rs`.
    pub(in crate::ingress) fn improvement_command_access(
        command: &ImprovementCommand,
    ) -> crate::bootstrap::restart::CommandAccess {
        match *command {}
    }
}
