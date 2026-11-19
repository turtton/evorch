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
        if let Command::Conversation(ConversationCommand::SendChat(send)) = command
            && self.service.root_recovery_pending(send.thread.as_str())
        {
            return Err(WorkbenchError::new(
                ErrorCode::Busy,
                "restart_busy: restoring conversation; retry the same request",
                RetryAdvice::SameRequest,
            ));
        }
        let access = command_access(command, |thread| self.chat_access(thread));
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

/// How a planned restart treats `command` (see `HostCommandPermit`): new work is
/// refused while the host prepares, input to work that already exists is
/// accepted until the safe point, and a stop always is. `chat_access` says which
/// of the first two a chat message is.
///
/// What an area's commands are is that area's own decision: every namespace
/// other than the ones classified here delegates to
/// `CommandHost::<area>_command_access` in its adapter, which for a namespace
/// without commands cannot be called, and which a PR adding the first command
/// must write. A command left to the default below is new work.
pub(super) fn command_access(
    command: &Command,
    chat_access: impl FnOnce(&str) -> CommandAccess,
) -> CommandAccess {
    match command {
        Command::Conversation(ConversationCommand::SendChat(send)) => {
            chat_access(send.thread.as_str())
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
        Command::Settings(c) => CommandHost::settings_command_access(c),
        Command::Auth(c) => CommandHost::auth_command_access(c),
        Command::Diagnostics(c) => CommandHost::diagnostics_command_access(c),
        Command::Memory(c) => CommandHost::memory_command_access(c),
        Command::Evaluation(c) => CommandHost::evaluation_command_access(c),
        Command::Improvement(c) => CommandHost::improvement_command_access(c),
        Command::PullRequests(c) => CommandHost::pull_requests_command_access(c),
        Command::Browser(c) => CommandHost::browser_command_access(c),
        Command::Artifacts(c) => CommandHost::artifacts_command_access(c),
        Command::Maintenance(c) => CommandHost::maintenance_command_access(c),
        _ => CommandAccess::NewWork,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workbench_protocol::commands::{
        AnswerUserQuestion, ApprovalDecision, ComposerRole, DecideToolApproval, SendChat, StopRun,
        ThreadTarget, WithdrawFollowUp,
    };
    use workbench_protocol::ids::{
        ApprovalId, ProjectId, QuestionId, RunId, SubmissionId, ThreadId,
    };
    use workbench_protocol::namespaces::{ApprovalsCommand, QuestionsCommand};
    use workbench_protocol::wire::{InstanceEpoch, Revision};

    fn name(access: CommandAccess) -> &'static str {
        match access {
            CommandAccess::NewWork => "new",
            CommandAccess::ExistingInput => "existing",
            CommandAccess::Stop => "stop",
        }
    }

    fn thread() -> ThreadTarget {
        ThreadTarget {
            thread: ThreadId::new("t").unwrap(),
        }
    }

    fn send_chat() -> Command {
        Command::Conversation(ConversationCommand::SendChat(SendChat {
            project: ProjectId::new("p").unwrap(),
            thread: ThreadId::new("t").unwrap(),
            submission_id: SubmissionId::new("s").unwrap(),
            composer_role: ComposerRole::Worker,
            text: "hello".into(),
            skills: vec![],
            attachments: vec![],
            model_preference: None,
            fork_seed: None,
        }))
    }

    #[test]
    fn existing_operations_keep_their_classification() {
        let not_chat = |_: &str| -> CommandAccess { panic!("not a chat message") };
        assert_eq!(
            name(command_access(
                &Command::Execution(ExecutionCommand::StopChat(thread())),
                not_chat
            )),
            "stop"
        );
        assert_eq!(
            name(command_access(
                &Command::Conversation(ConversationCommand::DeliverFollowUpsNextTurn(thread())),
                not_chat
            )),
            "existing"
        );
        assert_eq!(
            name(command_access(
                &Command::Execution(ExecutionCommand::StopRun(StopRun {
                    thread: ThreadId::new("t").unwrap(),
                    run: RunId::new("r").unwrap(),
                })),
                not_chat
            )),
            "stop"
        );
        assert_eq!(
            name(command_access(
                &Command::Conversation(ConversationCommand::WithdrawFollowUp(WithdrawFollowUp {
                    thread: ThreadId::new("t").unwrap(),
                    run: RunId::new("r").unwrap(),
                    submission_id: SubmissionId::new("s").unwrap(),
                })),
                not_chat
            )),
            "existing"
        );
        // Answering a question and deciding an approval feed work that already exists.
        assert_eq!(
            name(command_access(
                &Command::Questions(QuestionsCommand::AnswerUserQuestion(AnswerUserQuestion {
                    thread: ThreadId::new("t").unwrap(),
                    question_id: QuestionId::new("q").unwrap(),
                    answer: "yes".into(),
                    question_revision: Revision(1),
                })),
                not_chat
            )),
            "existing"
        );
        assert_eq!(
            name(command_access(
                &Command::Approvals(ApprovalsCommand::DecideToolApproval(DecideToolApproval {
                    approval_id: ApprovalId::new("a").unwrap(),
                    core_epoch: InstanceEpoch(1),
                    approval_revision: Revision(1),
                    decision: ApprovalDecision::Approve,
                })),
                not_chat
            )),
            "existing"
        );
        // A chat message is classified by the thread's run, and by nothing else.
        for access in [CommandAccess::NewWork, CommandAccess::ExistingInput] {
            let classified = command_access(&send_chat(), |thread| {
                assert_eq!(thread, "t");
                access
            });
            assert_eq!(name(classified), name(access));
        }
        // Everything else the host classifies stays new work.
        assert_eq!(
            name(command_access(
                &Command::Conversation(ConversationCommand::CompactChat(thread())),
                not_chat
            )),
            "new"
        );
    }
}
