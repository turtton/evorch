//! Evaluation namespace adapter: its queries read the evaluation files through
//! the `evaluation` area (PR 13); its commands start, stop and read the run,
//! report and rejudge jobs of that area (PR 17c).
use workbench_protocol::commands::{CommandDisposition, CommandReply, CommandResult};
use workbench_protocol::envelope::Provenance;
use workbench_protocol::errors::WorkbenchError;
use workbench_protocol::namespaces::evaluation::{
    EvaluationCommand, EvaluationQuery, EvaluationReply, EvaluationResult,
};
use workbench_protocol::queries::{Query, QueryReply};

use crate::ingress::CommandHost;

impl CommandHost {
    pub(in crate::ingress) fn evaluation_command(
        &mut self,
        command: EvaluationCommand,
        _provenance: &Provenance,
        restart: crate::bootstrap::restart::HostCommandPermit,
    ) -> Result<CommandReply, WorkbenchError> {
        let evaluation = &self.areas.evaluation;
        let accepted = match &command {
            EvaluationCommand::StartEvaluationRun(start) => evaluation.start_run(start, restart)?,
            EvaluationCommand::StartEvaluationReport(start) => {
                evaluation.start_report(start, restart)?
            }
            // Cancelling holds the restart gate only for this call; the job it
            // names keeps the permit of its own start.
            EvaluationCommand::CancelEvaluationJob(cancel) => {
                evaluation.cancel_job(&cancel.operation_id)?
            }
        };
        let mut reply = CommandReply::with_result(CommandResult::Evaluation(
            EvaluationResult::Job(Box::new(accepted.view)),
        ));
        if accepted.already_applied {
            reply.disposition = CommandDisposition::AlreadyApplied;
        }
        Ok(reply)
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
            EvaluationQuery::GetEvaluationJob(target) => {
                EvaluationReply::Job(Box::new(evaluation.job(&target.operation_id)?))
            }
        }))
    }
}

impl CommandHost {
    /// A start is new work: a preparing host refuses it, and one that was
    /// accepted settles before the host seals (a stop of the host ends it). A
    /// cancel only stops work, so a planned restart always lets it through.
    pub(in crate::ingress) fn evaluation_command_access(
        command: &EvaluationCommand,
    ) -> crate::bootstrap::restart::CommandAccess {
        use crate::bootstrap::restart::CommandAccess;
        match command {
            EvaluationCommand::StartEvaluationRun(_)
            | EvaluationCommand::StartEvaluationReport(_) => CommandAccess::NewWork,
            EvaluationCommand::CancelEvaluationJob(_) => CommandAccess::Stop,
        }
    }
}
