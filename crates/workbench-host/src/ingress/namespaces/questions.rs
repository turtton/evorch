//! Questions namespace adapter: answers to questions agents asked.
use workbench_protocol::commands::{AnswerUserQuestion, CommandReply};
use workbench_protocol::envelope::Provenance;
use workbench_protocol::errors::WorkbenchError;
use workbench_protocol::namespaces::QuestionsCommand;

use crate::commands::{HostCommand, RejectKind};
use crate::ingress::{CommandHost, reject_error};

impl CommandHost {
    /// Pending answers must quote the current routing revision. An
    /// already-saved identical answer remains an idempotent repeat.
    pub(in crate::ingress) fn questions_command(
        &mut self,
        command: QuestionsCommand,
        provenance: &Provenance,
    ) -> Result<CommandReply, WorkbenchError> {
        let QuestionsCommand::AnswerUserQuestion(AnswerUserQuestion {
            thread,
            question_id,
            answer,
            question_revision,
        }) = command;
        if let Ok(Some(question)) = self.service.runtime().user_question(question_id.as_str())
            && question.answer.is_none()
            && question.routing_revision != question_revision.get()
        {
            return Err(reject_error(
                RejectKind::Conflict,
                "question routing changed; refresh before answering",
            ));
        }
        self.run_host_command(
            HostCommand::AnswerUserQuestion {
                thread_id: thread.as_str().to_owned(),
                question_id: question_id.as_str().to_owned(),
                answer,
            },
            provenance,
        )
    }
}

impl CommandHost {
    /// Question listings are read from the projection.
    pub(in crate::ingress) fn questions_query(
        &self,
        query: &workbench_protocol::queries::Query,
        _namespace_query: &workbench_protocol::namespaces::QuestionsQuery,
    ) -> Result<workbench_protocol::queries::QueryReply, WorkbenchError> {
        self.projection.query(query)
    }
}
