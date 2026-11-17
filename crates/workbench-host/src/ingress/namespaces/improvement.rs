//! Improvement namespace adapter: a command or query only names its project;
//! the directory is resolved here, from the project directory the core owns. An
//! update is handed to the `improvement` area, which runs it as an operation.
use workbench_protocol::commands::{CommandDisposition, CommandReply, CommandResult};
use workbench_protocol::envelope::Provenance;
use workbench_protocol::errors::WorkbenchError;
use workbench_protocol::namespaces::improvement::{
    ImprovementCommand, ImprovementQuery, ImprovementReply, ImprovementResult,
};
use workbench_protocol::queries::{Query, QueryReply};

use crate::ingress::CommandHost;

impl CommandHost {
    pub(in crate::ingress) fn improvement_command(
        &mut self,
        command: ImprovementCommand,
        _provenance: &Provenance,
        restart: crate::bootstrap::restart::HostCommandPermit,
    ) -> Result<CommandReply, WorkbenchError> {
        let root = self.project_root(command.project())?;
        let submitted = self.areas.improvement.submit(&command, root, restart)?;
        let mut reply = CommandReply::with_result(CommandResult::Improvement(
            ImprovementResult::Operation(submitted.view),
        ));
        if submitted.already_applied {
            reply.disposition = CommandDisposition::AlreadyApplied;
        }
        Ok(reply)
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
            ImprovementQuery::GetImprovementUpdate(target) => {
                ImprovementReply::Update(Box::new(improvement.update_of(&target.operation_id)?))
            }
        }))
    }
}

impl CommandHost {
    /// Every improvement command writes a candidate (triage also a draft file):
    /// new work for a planned restart, like the memory commands.
    pub(in crate::ingress) fn improvement_command_access(
        _command: &ImprovementCommand,
    ) -> crate::bootstrap::restart::CommandAccess {
        crate::bootstrap::restart::CommandAccess::NewWork
    }
}
