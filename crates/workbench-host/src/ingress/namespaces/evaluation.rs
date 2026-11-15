//! Evaluation namespace adapter (read side, PR 13): the namespace names no
//! command; its queries read the evaluation files through the `evaluation` area.
use workbench_protocol::commands::CommandReply;
use workbench_protocol::envelope::Provenance;
use workbench_protocol::errors::WorkbenchError;
use workbench_protocol::namespaces::evaluation::{
    EvaluationCommand, EvaluationQuery, EvaluationReply,
};
use workbench_protocol::queries::{Query, QueryReply};

use crate::ingress::CommandHost;

impl CommandHost {
    pub(in crate::ingress) fn evaluation_command(
        &mut self,
        command: EvaluationCommand,
        _provenance: &Provenance,
        _restart: crate::bootstrap::restart::HostCommandPermit,
    ) -> Result<CommandReply, WorkbenchError> {
        match command {}
    }

    pub(in crate::ingress) fn evaluation_query(
        &self,
        _query: &Query,
        query: &EvaluationQuery,
    ) -> Result<QueryReply, WorkbenchError> {
        let evaluation = &self.areas.evaluation;
        Ok(QueryReply::Evaluation(match query {
            EvaluationQuery::ListEvaluations(list) => {
                EvaluationReply::List(Box::new(evaluation.list(list)?))
            }
            EvaluationQuery::GetEvaluation(get) => {
                EvaluationReply::Group(Box::new(evaluation.get(get)?))
            }
            EvaluationQuery::GetEvaluationReport(get) => {
                EvaluationReply::Report(Box::new(evaluation.report(get)?))
            }
            EvaluationQuery::GetEvaluationOperation(target) => {
                EvaluationReply::Operation(evaluation.operation(&target.operation_id)?)
            }
        }))
    }
}

impl CommandHost {
    /// How a planned restart treats this namespace's commands. The namespace has
    /// none yet, so this cannot be called; PR 17c, which adds the first command,
    /// writes the classification here (default: new work), not in the shared
    /// `restart.rs`.
    pub(in crate::ingress) fn evaluation_command_access(
        command: &EvaluationCommand,
    ) -> crate::bootstrap::restart::CommandAccess {
        match *command {}
    }
}
