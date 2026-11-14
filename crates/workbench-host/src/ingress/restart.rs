//! Command classification is shared by Unix, Web and the embedded adapter.
use super::*;
use crate::bootstrap::restart::{CommandAccess, HostCommandPermit};
use workbench_protocol::namespaces::{ConversationCommand, ExecutionCommand};
pub(super) fn busy() -> WorkbenchError {
    WorkbenchError::new(
        ErrorCode::Busy,
        "host is preparing restart; command was not accepted",
        RetryAdvice::UserActionRequired,
    )
}
impl CommandHost {
    pub(super) fn chat_access(&self, thread: &str) -> CommandAccess {
        if self
            .service
            .bindings
            .root_of(thread)
            .and_then(|run| self.service.runtime().inspect_agent(run).ok())
            .is_some_and(|run| {
                matches!(
                    run.phase,
                    event_bus::AgentRunPhase::Pending
                        | event_bus::AgentRunPhase::Running
                        | event_bus::AgentRunPhase::Waiting
                )
            })
        {
            CommandAccess::ExistingInput
        } else {
            CommandAccess::NewWork
        }
    }
    pub(super) fn restart_command_permit(
        &self,
        command: &Command,
    ) -> Result<HostCommandPermit, WorkbenchError> {
        let access = match command {
            Command::Conversation(ConversationCommand::SendChat(send)) => {
                self.chat_access(send.thread.as_str())
            }
            Command::Conversation(
                ConversationCommand::WithdrawFollowUp(_)
                | ConversationCommand::DeliverFollowUpsNextTurn(_),
            )
            | Command::Questions(_)
            | Command::Approvals(_) => CommandAccess::ExistingInput,
            Command::Execution(ExecutionCommand::StopChat(_) | ExecutionCommand::StopRun(_)) => {
                CommandAccess::Stop
            }
            _ => CommandAccess::NewWork,
        };
        self.service
            .restart_gate()
            .enter(access)
            .map_err(|_| busy())
    }
    pub(super) fn restart_local_permit(
        &self,
        command: &HostCommand,
    ) -> Result<HostCommandPermit, HostEvent> {
        let access = match command {
            HostCommand::SendChat(chat) => self.chat_access(&chat.thread_id),
            HostCommand::AnswerUserQuestion { .. }
            | HostCommand::DecideToolApproval { .. }
            | HostCommand::DeliverFollowUpsNextTurn { .. } => CommandAccess::ExistingInput,
            HostCommand::StopChat { .. } | HostCommand::StopRun { .. } => CommandAccess::Stop,
            _ => CommandAccess::NewWork,
        };
        self.service.restart_gate().enter(access).map_err(|_| {
            HostEvent::rejected(
                RejectKind::Busy,
                "host is preparing restart; command was not accepted",
            )
        })
    }
}
