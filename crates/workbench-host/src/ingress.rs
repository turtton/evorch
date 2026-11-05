//! The protocol entry point: [`CommandHost::handle_command`].
//!
//! Desktop, web and CLI clients all reach the runtime through here. A
//! `workbench_protocol::commands::Command` is checked, deduplicated by its
//! submission id, translated into the host's own [`HostCommand`] and run by the
//! same [`CommandService`] the in-process desktop adapter calls, so the ownership
//! check, the follow-up rule for a thread that is still running and every other
//! validation are the same whichever door the request came through.

use workbench_protocol::commands::{
    AnswerUserQuestion, Command, CommandDisposition, CommandReply, ComposerRole as WireRole,
    ContinueChat, Delegation, ModelPreference, ReferenceKind as WireReferenceKind, SendChat,
    SubmitGoal, WithdrawFollowUp,
};
use workbench_protocol::envelope::Provenance;
use workbench_protocol::errors::{ErrorCode, ErrorDetails, RetryAdvice, WorkbenchError};
use workbench_protocol::handshake::Capability;
use workbench_protocol::ids::{ProjectId, RequestId, SubmissionId};
use workbench_protocol::queries::{CommandReceipt, Query, QueryReply};

use crate::commands::{
    ChatInput, CommandService, ComposerRole, ContinueInput, GoalInput, HostCommand, HostEvent,
    PacketReference, ReferenceKind, RejectKind,
};
use crate::projects::{ProjectDirectory, StaticProjects};
use crate::receipts::{Receipts, SubmissionRecord};

/// The command service together with what the host remembers about commands.
pub struct CommandHost {
    /// The service every ingress shares. Public so the process that composes
    /// the host can bind threads and configure it, as the desktop adapter does.
    pub service: CommandService,
    receipts: Receipts,
    projects: Box<dyn ProjectDirectory>,
}

impl CommandHost {
    pub fn new(service: CommandService) -> Self {
        Self {
            service,
            receipts: Receipts::default(),
            projects: Box::new(StaticProjects::default()),
        }
    }

    pub fn with_projects(mut self, projects: impl ProjectDirectory + 'static) -> Self {
        self.projects = Box::new(projects);
        self
    }

    pub fn with_receipts(mut self, receipts: Receipts) -> Self {
        self.receipts = receipts;
        self
    }

    pub fn receipts(&self) -> &Receipts {
        &self.receipts
    }

    /// Run one protocol command for `provenance`, which the caller derived from
    /// the authenticated transport and session, never from the request.
    ///
    /// # Errors
    /// A structured error when the command is invalid, refused, or conflicts
    /// with an earlier submission that used the same id.
    pub fn handle_command(
        &mut self,
        command: Command,
        provenance: Provenance,
    ) -> Result<CommandReply, WorkbenchError> {
        provenance.validate()?;
        command.validate()?;
        match command {
            Command::SendChat(send) => self.send_chat(send, &provenance),
            Command::WithdrawFollowUp(withdraw) => self.withdraw_follow_up(&withdraw),
            Command::DecideToolApproval(_) => Err(approvals_unavailable()),
            other => {
                let host_command = self.translate(other)?;
                reply_from_events(self.service.submit(host_command), None)
            }
        }
    }

    /// Like [`Self::handle_command`], and remember the outcome under `request`
    /// so `GetCommandReceipt` can answer for a client that lost the reply.
    pub fn handle_request(
        &mut self,
        request: &RequestId,
        command: Command,
        provenance: Provenance,
    ) -> Result<CommandReply, WorkbenchError> {
        let result = self
            .handle_command(command, provenance.clone())
            .map_err(|error| error.for_request(request.clone()));
        self.receipts.record_request(request, &provenance, &result);
        result
    }

    /// The queries this host answers: today only what became of an earlier command.
    ///
    /// # Errors
    /// [`ErrorCode::UnsupportedCapability`] for every other query; the read side
    /// of the protocol is a later step.
    pub fn handle_query(&self, query: &Query) -> Result<QueryReply, WorkbenchError> {
        match query {
            Query::GetCommandReceipt { request_id } => {
                Ok(QueryReply::CommandReceipt(self.command_receipt(request_id)))
            }
            other => Err(WorkbenchError::new(
                ErrorCode::UnsupportedCapability,
                "this host does not serve that query yet",
                RetryAdvice::Never,
            )
            .with_details(ErrorDetails::MissingCapabilities {
                capabilities: vec![other.required_capability().into()],
            })),
        }
    }

    pub fn command_receipt(&self, request: &RequestId) -> CommandReceipt {
        self.receipts.command_receipt(request)
    }

    /// Run a command from an in-process client (the desktop adapter) as
    /// `provenance`. It goes through the same service as a protocol command; a
    /// chat that carries a submission id is remembered with its provenance.
    pub fn submit_local(
        &mut self,
        command: HostCommand,
        provenance: &Provenance,
    ) -> Vec<HostEvent> {
        let submission = match &command {
            HostCommand::SendChat(chat) if !chat.submission_id.is_empty() => {
                Some(chat.submission_id.clone())
            }
            _ => None,
        };
        let events = self.service.submit(command);
        self.note_local_chat(submission, &events, provenance);
        events
    }

    /// [`CommandService::submit_chat_with_permit`] for an in-process client.
    pub fn submit_local_chat_with_permit(
        &mut self,
        chat: ChatInput,
        permit: runtime::ownership::OwnerPermit,
        provenance: &Provenance,
    ) -> Vec<HostEvent> {
        let submission = (!chat.submission_id.is_empty()).then(|| chat.submission_id.clone());
        let events = self.service.submit_chat_with_permit(chat, permit);
        self.note_local_chat(submission, &events, provenance);
        events
    }

    /// Who sent the accepted chat named `submission`.
    pub fn submission_provenance(&self, submission: &str) -> Option<&Provenance> {
        self.receipts.submission(submission).map(|r| &r.sent_by)
    }

    fn note_local_chat(
        &mut self,
        submission: Option<String>,
        events: &[HostEvent],
        provenance: &Provenance,
    ) {
        let Some(submission) = submission else {
            return;
        };
        let Some(run) = events.iter().find_map(|event| match event {
            HostEvent::ChatAccepted { run_id, .. } => Some(run_id.clone()),
            _ => None,
        }) else {
            return;
        };
        self.receipts.record_submission(
            &submission,
            SubmissionRecord {
                payload: None,
                reply: CommandReply {
                    disposition: CommandDisposition::Accepted,
                    submission_id: SubmissionId::new(submission.clone()).ok(),
                    run_id: workbench_protocol::ids::RunId::new(run).ok(),
                    thread_revision: None,
                },
                sent_by: provenance.clone(),
            },
        );
    }

    fn send_chat(
        &mut self,
        send: SendChat,
        provenance: &Provenance,
    ) -> Result<CommandReply, WorkbenchError> {
        if let Some(existing) = self.earlier_submission(&send)? {
            return Ok(existing);
        }
        let input = self.chat_input(&send)?;
        let reply = reply_from_events(
            self.service.submit(HostCommand::SendChat(input)),
            Some(send.submission_id.clone()),
        )?;
        let id = send.submission_id.as_str().to_owned();
        self.receipts.record_submission(
            &id,
            SubmissionRecord {
                payload: Some(send),
                reply: reply.clone(),
                sent_by: provenance.clone(),
            },
        );
        Ok(reply)
    }

    /// The earlier outcome of `send`'s submission id: the recorded reply for an
    /// identical resend, [`ErrorCode::Conflict`] when the id was used for
    /// something else, `None` for a new submission. Memory first; after a
    /// restart the runtime's durable input receipt answers.
    fn earlier_submission(&self, send: &SendChat) -> Result<Option<CommandReply>, WorkbenchError> {
        let id = send.submission_id.as_str();
        if let Some(SubmissionRecord {
            payload: Some(payload),
            reply,
            ..
        }) = self.receipts.submission(id)
        {
            return if payload == send {
                Ok(Some(already_applied(reply.clone())))
            } else {
                Err(submission_conflict(&send.submission_id))
            };
        }
        let ids = [id.to_owned()];
        let storage_error = |error: storage::StorageError| {
            WorkbenchError::new(
                ErrorCode::StorageUnavailable,
                format!("could not read input receipts: {error}"),
                RetryAdvice::SameRequest,
            )
        };
        let runtime = self.service.runtime();
        let Some(receipt) = runtime
            .host_input_receipts(&ids)
            .map_err(storage_error)?
            .into_iter()
            .next()
        else {
            return Ok(None);
        };
        let origin = runtime
            .host_input_origins(&ids)
            .map_err(storage_error)?
            .into_iter()
            .next();
        let same_text = origin
            .as_ref()
            .map_or(receipt.text == send.text, |origin| origin.text == send.text);
        // Skill names that could not be read cannot disprove a match.
        let same_skills = origin
            .as_ref()
            .is_none_or(|origin| origin.unreadable.is_some() || origin.skills == send.skills);
        let same_images = receipt.image_count as usize == send.images.len();
        if receipt.provenance == storage::HostInputProvenance::User
            && same_text
            && same_skills
            && same_images
        {
            Ok(Some(CommandReply {
                disposition: CommandDisposition::AlreadyApplied,
                submission_id: Some(send.submission_id.clone()),
                run_id: workbench_protocol::ids::RunId::new(receipt.run_id).ok(),
                thread_revision: None,
            }))
        } else {
            Err(submission_conflict(&send.submission_id))
        }
    }

    fn project_root(&self, project: &ProjectId) -> Result<std::path::PathBuf, WorkbenchError> {
        self.projects.root_of(project).ok_or_else(|| {
            WorkbenchError::new(
                ErrorCode::NotFound,
                format!("unknown project `{}`", project.as_str().escape_debug()),
                RetryAdvice::Never,
            )
        })
    }

    fn chat_input(&self, send: &SendChat) -> Result<ChatInput, WorkbenchError> {
        if !send.images.is_empty() {
            return Err(unsupported(
                "image attachments are not available through the protocol yet",
            ));
        }
        Ok(ChatInput {
            submission_id: send.submission_id.as_str().to_owned(),
            project_root: Some(self.project_root(&send.project)?),
            composer_role: composer_role(send.composer_role),
            images: Vec::new(),
            thread_id: send.thread.as_str().to_owned(),
            text: send.text.clone(),
            skills: send.skills.clone(),
            model_preference: send.model_preference.as_ref().map(model_preference),
            fork_seed: send.fork_seed.as_ref().map(|seed| runtime::ChatForkSeed {
                source_run_id: seed.source_run_id.as_str().to_owned(),
                context_len: seed.context_len.get(),
            }),
        })
    }

    fn translate(&self, command: Command) -> Result<HostCommand, WorkbenchError> {
        Ok(match command {
            Command::ContinueChat(ContinueChat {
                project,
                thread,
                composer_role: role,
                model_preference: preference,
            }) => HostCommand::ContinueChat(ContinueInput {
                project_root: Some(self.project_root(&project)?),
                thread_id: thread.as_str().to_owned(),
                composer_role: composer_role(role),
                model_preference: preference.as_ref().map(model_preference),
                // Automatic continuations are the core's own decision.
                automatic: false,
            }),
            Command::CompactChat(target) => HostCommand::CompactChat {
                thread_id: target.thread.as_str().to_owned(),
            },
            Command::StopChat(target) => HostCommand::StopChat {
                thread_id: target.thread.as_str().to_owned(),
            },
            Command::StopRun(stop) => HostCommand::StopRun {
                thread_id: stop.thread.as_str().to_owned(),
                run_id: stop.run.as_str().to_owned(),
            },
            Command::DeliverFollowUpsNextTurn(target) => HostCommand::DeliverFollowUpsNextTurn {
                thread_id: target.thread.as_str().to_owned(),
            },
            Command::SubmitGoal(goal) => HostCommand::SubmitGoal(self.goal_input(&goal)?),
            Command::SetGoalReview(set) => HostCommand::SetGoalReview {
                thread_id: set.thread.as_str().to_owned(),
                goal_id: set.goal.as_str().to_owned(),
                enabled: set.enabled,
            },
            Command::SetGoalChecksPaused(set) => HostCommand::SetGoalChecksPaused {
                thread_id: set.thread.as_str().to_owned(),
                goal_id: set.goal.as_str().to_owned(),
                paused: set.paused,
            },
            Command::AnswerUserQuestion(AnswerUserQuestion {
                thread,
                question_id,
                answer,
                // The runtime does not version questions yet; a question that
                // was answered meanwhile is refused as already answered.
                question_revision: _,
            }) => HostCommand::AnswerUserQuestion {
                thread_id: thread.as_str().to_owned(),
                question_id: question_id.as_str().to_owned(),
                answer,
            },
            Command::SetAgentWebToolsEnabled(set) => HostCommand::SetWebToolsEnabled {
                enabled: set.enabled,
            },
            Command::SendChat(_)
            | Command::WithdrawFollowUp(_)
            | Command::DecideToolApproval(_) => {
                unreachable!("handled before translation")
            }
        })
    }

    fn goal_input(&self, goal: &SubmitGoal) -> Result<GoalInput, WorkbenchError> {
        match &goal.delegation {
            None | Some(Delegation::Direct) => {}
            // The desktop's delegation value starts a team; the protocol's
            // reference names a packet or issue to resolve. Mapping one onto the
            // other would start something the client did not ask for.
            Some(Delegation::Reference { .. }) => {
                return Err(unsupported(
                    "delegating a goal to a reference is not available yet",
                ));
            }
        }
        Ok(GoalInput {
            project_root: Some(self.project_root(&goal.project)?),
            delegation_value: None,
            project_id: goal.project.as_str().to_owned(),
            thread_id: goal.thread.as_str().to_owned(),
            goal: goal.objective.clone(),
            references: goal
                .references
                .iter()
                .map(|reference| PacketReference {
                    kind: match reference.kind {
                        WireReferenceKind::Packet => ReferenceKind::Packet,
                        WireReferenceKind::Issue => ReferenceKind::Issue,
                    },
                    value: reference.value.clone(),
                })
                .collect(),
            constraints: goal.constraints.clone(),
        })
    }

    fn withdraw_follow_up(
        &mut self,
        withdraw: &WithdrawFollowUp,
    ) -> Result<CommandReply, WorkbenchError> {
        let thread = withdraw.thread.as_str();
        if let Some(ownership) = self.service.ownership() {
            ownership
                .owned_permit(thread)
                .map_err(|error| reject_error(RejectKind::Fenced, &error.to_string()))?;
        }
        let bound = self.service.bindings.root_of(thread);
        if bound.map(|run| run.to_string()).as_deref() != Some(withdraw.run.as_str()) {
            return Err(reject_error(
                RejectKind::NotFound,
                "the run is not the active conversation root of this thread",
            ));
        }
        match self
            .service
            .withdraw_follow_up(thread, withdraw.submission_id.as_str())
        {
            Ok(()) => Ok(CommandReply {
                disposition: CommandDisposition::Accepted,
                submission_id: Some(withdraw.submission_id.clone()),
                run_id: Some(withdraw.run.clone()),
                thread_revision: None,
            }),
            Err(runtime::RuntimeError::FollowUpWithdrawal(
                runtime::FollowUpWithdrawalError::Unknown,
            )) => Err(reject_error(RejectKind::NotFound, "unknown queued message")),
            Err(runtime::RuntimeError::FollowUpWithdrawal(error)) => {
                Err(reject_error(RejectKind::Conflict, &error.to_string()))
            }
            Err(error) => Err(reject_error(RejectKind::Failed, &error.to_string())),
        }
    }
}

fn composer_role(role: WireRole) -> ComposerRole {
    match role {
        WireRole::Worker => ComposerRole::Worker,
        WireRole::Orchestrator => ComposerRole::Orchestrator,
    }
}

fn model_preference(preference: &ModelPreference) -> runtime::ModelPreference {
    runtime::ModelPreference {
        profile: preference.profile.clone(),
        model: preference.model.clone(),
        reasoning_effort: preference.reasoning_effort.clone(),
    }
}

fn already_applied(mut reply: CommandReply) -> CommandReply {
    reply.disposition = CommandDisposition::AlreadyApplied;
    reply
}

fn submission_conflict(id: &SubmissionId) -> WorkbenchError {
    WorkbenchError::new(
        ErrorCode::Conflict,
        format!(
            "submission `{}` was already used for a different message",
            id.as_str().escape_debug()
        ),
        RetryAdvice::UserActionRequired,
    )
}

fn unsupported(message: &str) -> WorkbenchError {
    reject_error(RejectKind::Unsupported, message)
}

/// Approval decisions are validated against the approval registry (ADR 0045
/// P4), which the host does not have yet. Refusing is safer than guessing which
/// pending tool call an approval id names.
fn approvals_unavailable() -> WorkbenchError {
    WorkbenchError::new(
        ErrorCode::UnsupportedCapability,
        "tool approval decisions are not available until the approval registry lands",
        RetryAdvice::Never,
    )
    .with_details(ErrorDetails::MissingCapabilities {
        capabilities: vec![Capability::Approvals.into()],
    })
}

/// The protocol error for a refusal the command service reported.
pub fn reject_error(kind: RejectKind, reason: &str) -> WorkbenchError {
    let (code, retry) = match kind {
        RejectKind::Fenced => (ErrorCode::PermissionDenied, RetryAdvice::UserActionRequired),
        RejectKind::NotFound => (ErrorCode::NotFound, RetryAdvice::Never),
        RejectKind::Conflict => (ErrorCode::Conflict, RetryAdvice::AfterResync),
        RejectKind::AlreadyResolved => (ErrorCode::AlreadyResolved, RetryAdvice::Never),
        RejectKind::Busy => (ErrorCode::Busy, RetryAdvice::SameRequest),
        RejectKind::Unsupported => (ErrorCode::UnsupportedCapability, RetryAdvice::Never),
        RejectKind::Invalid => (ErrorCode::InvalidRequest, RetryAdvice::Never),
        RejectKind::Failed => (ErrorCode::Internal, RetryAdvice::Never),
    };
    WorkbenchError::new(code, reason, retry)
}

/// The reply for what a command produced: the first refusal is the error; a
/// notice that reports nothing took effect is a conflict; otherwise accepted.
fn reply_from_events(
    events: Vec<HostEvent>,
    submission_id: Option<SubmissionId>,
) -> Result<CommandReply, WorkbenchError> {
    let mut reply = CommandReply {
        disposition: CommandDisposition::Accepted,
        submission_id,
        run_id: None,
        thread_revision: None,
    };
    let mut not_applied = None;
    let mut applied = false;
    for event in events {
        match event {
            HostEvent::ChatAccepted { run_id, .. } | HostEvent::ChatStopped { run_id, .. } => {
                reply.run_id = workbench_protocol::ids::RunId::new(run_id).ok();
                applied = true;
            }
            HostEvent::ChatRejected { reason, kind, .. }
            | HostEvent::CommandRejected { reason, kind } => {
                return Err(reject_error(kind, &reason));
            }
            HostEvent::ChatNotice {
                text,
                outcome: crate::commands::NoticeOutcome::NotApplied,
                ..
            } => {
                not_applied.get_or_insert(text);
            }
            HostEvent::ChatNotice { .. }
            | HostEvent::UserAnswerSaved { .. }
            | HostEvent::GoalAccepted { .. } => applied = true,
        }
    }
    match not_applied {
        Some(text) if !applied => Err(reject_error(RejectKind::Conflict, &text)),
        _ => Ok(reply),
    }
}
