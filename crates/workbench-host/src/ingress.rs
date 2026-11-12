//! The entry points of the host: [`CommandHost`].
//!
//! Desktop, web and CLI clients all reach the runtime through here, by one of two
//! doors that share everything after the translation:
//!
//! - [`CommandHost::handle_command`] takes a `workbench_protocol::commands::Command`
//!   and the [`Provenance`] its transport derived;
//! - [`CommandHost::submit_local`] takes the host's own [`HostCommand`] from an
//!   in-process client (the desktop adapter).
//!
//! Both do the same three steps in the same order: ownership first
//! ([`CommandService::authorize`]), then the submission ledger (a chat whose
//! submission id was seen is applied once), then the [`CommandService`]. The
//! service's execution entry points are crate-private, so this type is the only
//! way to run a command and none of the steps can be skipped.

mod namespaces;

use workbench_protocol::commands::{
    Command, CommandDisposition, CommandReply, ComposerRole as WireRole, Delegation,
    ModelPreference, ReferenceKind as WireReferenceKind, SendChat, SubmitGoal, WithdrawFollowUp,
};
use workbench_protocol::envelope::Provenance;
use workbench_protocol::errors::{ErrorCode, RetryAdvice, WorkbenchError};
use workbench_protocol::ids::{ProjectId, RequestId, SessionId, SubmissionId};
use workbench_protocol::namespaces::QuestionsCommand;
use workbench_protocol::queries::{CommandReceipt, Query, QueryReply};

use crate::commands::{
    ChatInput, CommandService, ComposerRole, GoalInput, HostCommand, HostEvent, NoticeOutcome,
    PacketReference, ReferenceKind, RejectKind,
};
use crate::projects::{ProjectDirectory, StaticProjects};
use crate::receipts::fingerprint::chat_fingerprint;
use crate::receipts::{Receipts, SubmissionRecord};

/// The command service together with what the host remembers about commands.
pub struct CommandHost {
    service: CommandService,
    conversation: namespaces::conversation::ConversationState,
    pub(crate) lane_sidebar_path: Option<std::path::PathBuf>,
    receipts: Receipts,
    projects: Box<dyn ProjectDirectory>,
    projection: std::sync::Arc<crate::projection::Projection>,
    /// The services behind the namespaces that later PRs fill in; see [`crate::areas`].
    pub(crate) areas: crate::areas::Areas,
}

/// What the host knows about a submission id.
enum Seen {
    New,
    /// Only the runtime's input receipt exists: the words, not the identity.
    Unverifiable,
    /// The conversation it went to was deleted.
    Deleted,
    Known {
        fingerprint: String,
        run_id: Option<String>,
        sent_by: Option<Provenance>,
    },
}

/// What running a command came to.
enum Executed {
    /// A chat whose submission id was accepted before, with the same content. It
    /// was not applied again.
    Duplicate {
        thread_id: String,
        run_id: Option<String>,
    },
    Events(Vec<HostEvent>),
}

impl CommandHost {
    /// Reserve an absent thread for a new chat, after resolving its project.
    /// This never claims or hands off an existing owner row, even a released one.
    pub fn reserve_new_chat_thread(&self, send: &SendChat) -> Result<(), WorkbenchError> {
        self.reserve_new_thread(
            &send.project,
            &send.thread,
            &send.text,
            send.composer_role == workbench_protocol::commands::ComposerRole::Orchestrator,
            send.fork_seed.as_ref().map(|seed| {
                (
                    seed.source_run_id.to_string(),
                    u64::from(seed.context_len.get()),
                )
            }),
        )
    }

    fn reserve_new_thread(
        &self,
        project: &ProjectId,
        thread: &workbench_protocol::ids::ThreadId,
        title_hint: &str,
        orchestrator: bool,
        fork: Option<(String, u64)>,
    ) -> Result<(), WorkbenchError> {
        let root = self.project_root(project)?;
        if let Some(owner) = self.service.ownership() {
            match owner.attach(thread.as_str()) {
                Ok(_) => (),
                Err(runtime::ownership::RegistryError::Absent) => {
                    owner.start(thread.as_str()).map_err(|_| {
                        reject_error(RejectKind::Fenced, "thread could not be reserved")
                    })?;
                }
                Err(_) => {
                    return Err(reject_error(
                        RejectKind::Fenced,
                        "thread ownership unavailable",
                    ));
                }
            }
        }
        // The conversation is the core's own now: list it in the sidebar, under
        // the same lock and file as every other sidebar write. Idempotent, and
        // nothing when the workspace service is not wired (a desktop that lists
        // its own threads).
        self.areas
            .workspace
            .register_thread(&crate::areas::workspace::NewThread {
                project,
                thread,
                title_hint,
                project_root: Some(root),
                orchestrator,
                fork,
            })
    }

    pub fn new(service: CommandService) -> Self {
        let projection = crate::projection::Projection::new(
            service.runtime().clone(),
            service.handle(),
            Default::default(),
        );
        Self {
            service,
            conversation: Default::default(),
            lane_sidebar_path: None,
            projection,
            areas: Default::default(),
            receipts: Receipts::default(),
            projects: Box::new(StaticProjects::default()),
        }
    }

    pub fn with_projects(mut self, projects: impl ProjectDirectory + 'static) -> Self {
        self.projection.configure(projects.entries());
        self.projects = Box::new(projects);
        self
    }

    /// Makes the project directory and the projection follow the projects the
    /// workspace service holds: the roots runs work in, and the saved names.
    ///
    /// # Errors
    /// The sidebar could not be read, or the projection could not be read again.
    pub(crate) fn sync_projects(&mut self) -> Result<(), WorkbenchError> {
        let entries = self.areas.workspace.project_entries()?;
        let mut names = std::collections::BTreeMap::new();
        for (id, root, name) in entries {
            self.projects.upsert(&id, root);
            names.insert(id.as_str().to_owned(), name);
        }
        self.projection
            .configure_projects(self.projects.entries(), names)
    }

    /// Install the area services, constructed from the bootstrap's handles
    /// (the projection they publish through is [`Self::projection`]). The
    /// projects the saved sidebar holds join the project directory.
    pub fn with_areas(mut self, areas: crate::areas::Areas) -> Self {
        self.areas = areas;
        if let Err(error) = self.sync_projects() {
            tracing::warn!(?error.code, "the saved projects were not read into the project directory");
        }
        self
    }

    pub fn with_receipts(mut self, receipts: Receipts) -> Self {
        self.receipts = receipts;
        self
    }

    /// The service behind the host, for what is not a command: binding threads,
    /// configuring the runtime, reading state. Commands run only through the host.
    pub fn service(&self) -> &CommandService {
        &self.service
    }

    pub fn service_mut(&mut self) -> &mut CommandService {
        &mut self.service
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn receipts(&self) -> &Receipts {
        &self.receipts
    }

    /// Run one protocol command for `provenance`, which the caller derived from
    /// the authenticated transport and session, never from the request.
    ///
    /// A chat is deduplicated by its submission id: the same message again is
    /// answered [`CommandDisposition::AlreadyApplied`] without a second run, a
    /// different message under the same id is [`ErrorCode::Conflict`].
    ///
    /// Pending answers must quote the current routing revision. An already-saved
    /// identical answer remains an idempotent repeat.
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
        self.dispatch_command(command, &provenance)
    }

    /// Run a command through ownership, the submission ledger and the command
    /// service, and report what it produced.
    fn run_host_command(
        &mut self,
        command: HostCommand,
        provenance: &Provenance,
    ) -> Result<CommandReply, WorkbenchError> {
        let events = self.authorize_and_execute(command, provenance)?;
        reply_from_events(events, None)
    }

    /// Like [`Self::handle_command`], and remember the outcome under `session` and
    /// `request` so `GetCommandReceipt` can answer for a client that lost the reply.
    pub fn handle_request(
        &mut self,
        session: &SessionId,
        request: &RequestId,
        command: Command,
        provenance: Provenance,
    ) -> Result<CommandReply, WorkbenchError> {
        let subject = match &command {
            Command::Questions(QuestionsCommand::AnswerUserQuestion(q)) => self
                .service
                .runtime()
                .user_question(q.question_id.as_str())
                .ok()
                .flatten()
                .map(|question| {
                    let thread = self
                        .service
                        .runtime()
                        .with_storage_reader(|db| {
                            Ok(db.thread_of_run(&question.run_id)?.map(|r| r.thread_id))
                        })
                        .ok()
                        .flatten()
                        .flatten();
                    (Some(question.run_id), thread)
                }),
            _ => None,
        };
        let result = self
            .handle_command(command, provenance.clone())
            .map_err(|error| error.for_request(request.clone()));
        let recorded = self
            .receipts
            .record_request(session, request, &provenance, &result);
        if recorded && let Some((run, thread)) = subject {
            self.receipts
                .associate_request(session, request, run, thread);
        }
        result
    }

    /// Shared transport-independent read and subscription service.
    pub fn projection(&self) -> &std::sync::Arc<crate::projection::Projection> {
        &self.projection
    }

    pub fn handle_query(
        &self,
        session: &SessionId,
        query: &Query,
    ) -> Result<QueryReply, WorkbenchError> {
        self.dispatch_query(session, query)
    }

    /// Complete immutable arguments for an authenticated human's confirmation.
    /// List summaries are not sufficient evidence for offering Approve.
    pub fn full_approval_input(
        &self,
        approval_id: &workbench_protocol::ids::ApprovalId,
        epoch: workbench_protocol::wire::InstanceEpoch,
        revision: workbench_protocol::wire::Revision,
        provenance: &Provenance,
    ) -> Result<Option<std::sync::Arc<serde_json::Value>>, WorkbenchError> {
        provenance.validate()?;
        if provenance.actor != workbench_protocol::envelope::Actor::Human {
            return Err(reject_error(
                RejectKind::Fenced,
                "approval input requires human ingress",
            ));
        }
        self.approval_input_value(approval_id, epoch, revision, approval_error)
    }

    fn approval_input_value(
        &self,
        approval_id: &workbench_protocol::ids::ApprovalId,
        epoch: workbench_protocol::wire::InstanceEpoch,
        revision: workbench_protocol::wire::Revision,
        map_error: fn(sandbox::approval::ApprovalError) -> WorkbenchError,
    ) -> Result<Option<std::sync::Arc<serde_json::Value>>, WorkbenchError> {
        let registry = self.projection.approval_registry();
        let id = sandbox::approval::ApprovalId {
            core_epoch: epoch.get(),
            value: approval_id.as_str().to_owned(),
        };
        if let Some(approval) = registry.get(&id) {
            if !self
                .service
                .runtime()
                .deleted_runs(&[approval.run_id])
                .map_err(crate::queries::storage_error)?
                .is_empty()
            {
                return Err(reject_error(
                    RejectKind::NotFound,
                    "approval conversation was deleted",
                ));
            }
            if let (Some(ownership), Some(thread)) = (self.service.ownership(), approval.thread_id)
            {
                ownership.owned_permit(&thread).map_err(|_| {
                    reject_error(RejectKind::Fenced, "approval conversation is not owned")
                })?;
            }
        }
        registry.full_input(&id, revision.get()).map_err(map_error)
    }

    fn decide_approval(
        &self,
        decide: &workbench_protocol::commands::DecideToolApproval,
        provenance: &Provenance,
    ) -> Result<CommandReply, WorkbenchError> {
        use sandbox::approval::{
            ApprovalActor, ApprovalDecision, ApprovalId, ApprovalProvenance, ApprovalSurface,
        };
        use workbench_protocol::envelope::{Actor, Surface};
        if provenance.actor != Actor::Human {
            return Err(reject_error(
                RejectKind::Fenced,
                "tool approvals require human ingress",
            ));
        }
        let registry = self.projection.approval_registry();
        let id = ApprovalId {
            core_epoch: decide.core_epoch.get(),
            value: decide.approval_id.as_str().to_owned(),
        };
        if let Some(approval) = registry.get(&id) {
            if !self
                .service
                .runtime()
                .deleted_runs(std::slice::from_ref(&approval.run_id))
                .map_err(crate::queries::storage_error)?
                .is_empty()
            {
                return Err(reject_error(
                    RejectKind::NotFound,
                    "approval conversation was deleted",
                ));
            }
            if let (Some(ownership), Some(thread)) = (self.service.ownership(), &approval.thread_id)
            {
                ownership.owned_permit(thread).map_err(|_| {
                    reject_error(RejectKind::Fenced, "approval conversation is not owned")
                })?;
            }
        }
        let decided = registry
            .resolve(
                &id,
                decide.approval_revision.get(),
                match decide.decision {
                    workbench_protocol::commands::ApprovalDecision::Approve => {
                        ApprovalDecision::Approve
                    }
                    workbench_protocol::commands::ApprovalDecision::Deny => ApprovalDecision::Deny,
                },
                ApprovalProvenance {
                    actor: ApprovalActor::Human,
                    surface: match provenance.surface {
                        Surface::Web => ApprovalSurface::Web,
                        Surface::Desktop => ApprovalSurface::Desktop,
                        Surface::Cli => ApprovalSurface::Cli,
                        Surface::None => ApprovalSurface::None,
                    },
                },
            )
            .map_err(approval_error)?;
        if let Err(error) = self.projection.approval_changed(&decided.id) {
            tracing::warn!(code = ?error.code, "approval resolved; projection refresh failed");
        }
        Ok(CommandReply::accepted())
    }

    /// What became of `request` in `session`. An applied request whose submission or
    /// run was deleted since reads as unknown: deletion removes what the host knew, and
    /// the reply names the deleted run.
    ///
    /// # Errors
    /// [`ErrorCode::StorageUnavailable`] when deletion cannot be checked.
    pub fn command_receipt(
        &self,
        session: &SessionId,
        request: &RequestId,
    ) -> Result<CommandReceipt, WorkbenchError> {
        let receipt = self.receipts.command_receipt(session, request);
        let deleted = self
            .request_subject_deleted(session, request)
            .map_err(|error| {
                reject_error(
                    RejectKind::Unavailable,
                    &format!("could not check the request's conversation: {error}"),
                )
            })?;
        Ok(if deleted {
            CommandReceipt {
                request_id: receipt.request_id,
                state: workbench_protocol::queries::CommandReceiptState::Unknown,
            }
        } else {
            receipt
        })
    }

    /// Who sent `request`; `None` for an unknown request and for one whose submission or
    /// run was deleted.
    pub fn request_provenance(
        &self,
        session: &SessionId,
        request: &RequestId,
    ) -> Option<Provenance> {
        if self.request_subject_deleted(session, request).ok()? {
            return None;
        }
        self.receipts.request_provenance(session, request).cloned()
    }

    fn request_subject_deleted(
        &self,
        session: &SessionId,
        request: &RequestId,
    ) -> Result<bool, storage::StorageError> {
        let Some(subject) = self.receipts.request_subject(session, request) else {
            return Ok(false);
        };
        if let Some(thread) = &subject.thread
            && self
                .service
                .runtime()
                .with_storage_reader(|db| Ok(db.tombstones()?.contains_thread(thread)))?
                .unwrap_or(false)
        {
            return Ok(true);
        }
        if let Some(run) = &subject.run
            && !self
                .service
                .runtime()
                .deleted_runs(std::slice::from_ref(run))?
                .is_empty()
        {
            return Ok(true);
        }
        if let Some(submission) = &subject.submission {
            return Ok(matches!(self.submission_status(submission)?, Seen::Deleted));
        }
        Ok(false)
    }

    /// Run a command from an in-process client (the desktop adapter) as
    /// `provenance`. It passes the same ownership check and submission ledger as a
    /// protocol command: a chat resent under a used submission id is not applied
    /// twice, and the first answer saved to a question is remembered with who
    /// saved it.
    pub fn submit_local(
        &mut self,
        command: HostCommand,
        provenance: &Provenance,
    ) -> Vec<HostEvent> {
        match self.service.authorize(&command) {
            Ok(permit) => self.execute_local(command, permit, provenance),
            Err(refusal) => vec![refusal],
        }
    }

    /// [`Self::submit_local`] for a chat sent under a permit the caller already
    /// holds; the permit is checked against the thread's current ownership first.
    pub fn submit_local_chat_with_permit(
        &mut self,
        chat: ChatInput,
        permit: runtime::ownership::OwnerPermit,
        provenance: &Provenance,
    ) -> Vec<HostEvent> {
        if let Err(refusal) = self.service.authorize_chat_permit(&chat, &permit) {
            return vec![refusal];
        }
        self.execute_local(HostCommand::SendChat(chat), Some(permit), provenance)
    }

    /// [`Self::submit_local`] under a permit the caller already authorised (or
    /// none without an ownership host), skipping the lookup. Skipping the lookup is
    /// only sound for a test that built the permit itself, so this exists only for
    /// tests (feature `test-support`).
    #[cfg(any(test, feature = "test-support"))]
    pub fn submit_local_authorized(
        &mut self,
        command: HostCommand,
        permit: Option<runtime::ownership::OwnerPermit>,
        provenance: &Provenance,
    ) -> Vec<HostEvent> {
        self.execute_local(command, permit, provenance)
    }

    /// Who sent the accepted chat named `submission`, from memory or from storage.
    /// `None` for an unknown id and for one whose run was deleted: deletion removes
    /// what the host knew about the message.
    pub fn submission_provenance(&self, submission: &str) -> Option<Provenance> {
        match self.submission_status(submission).ok()? {
            Seen::Known { sent_by, .. } => sent_by,
            Seen::New | Seen::Unverifiable | Seen::Deleted => None,
        }
    }

    /// Who saved the answer to `question`, if this host saw it saved. An answer
    /// stored before this host started has no provenance here; it is never
    /// credited to whoever repeats it. `None` once the question was deleted.
    pub fn answer_provenance(&self, question: &str) -> Option<Provenance> {
        let runtime = self.service.runtime();
        if runtime.has_run_store() && !matches!(runtime.user_question(question), Ok(Some(_))) {
            return None;
        }
        self.receipts.answer_provenance(question).cloned()
    }

    fn execute_local(
        &mut self,
        command: HostCommand,
        permit: Option<runtime::ownership::OwnerPermit>,
        provenance: &Provenance,
    ) -> Vec<HostEvent> {
        match self.execute(command, permit, provenance) {
            Executed::Duplicate { thread_id, run_id } => vec![HostEvent::ChatAccepted {
                thread_id,
                run_id: run_id.unwrap_or_default(),
            }],
            Executed::Events(events) => events,
        }
    }

    fn authorize_and_execute(
        &mut self,
        command: HostCommand,
        provenance: &Provenance,
    ) -> Result<Vec<HostEvent>, WorkbenchError> {
        let permit = self.service.authorize(&command).map_err(refusal_error)?;
        match self.execute(command, permit, provenance) {
            Executed::Events(events) => Ok(events),
            Executed::Duplicate { .. } => unreachable!("only a chat can be a duplicate"),
        }
    }

    /// Deduplicate, run and record. The caller has authorised `permit`.
    fn execute(
        &mut self,
        command: HostCommand,
        permit: Option<runtime::ownership::OwnerPermit>,
        provenance: &Provenance,
    ) -> Executed {
        let submission = match &command {
            HostCommand::SendChat(chat) if !chat.submission_id.is_empty() => Some(chat.clone()),
            _ => None,
        };
        if let Some(chat) = &submission {
            match self.earlier_submission(chat) {
                Ok(None) => {}
                Ok(Some(run_id)) => {
                    return Executed::Duplicate {
                        thread_id: chat.thread_id.clone(),
                        run_id,
                    };
                }
                Err(refusal) => {
                    return Executed::Events(vec![HostEvent::chat_rejected(
                        chat.thread_id.clone(),
                        refusal.0,
                        refusal.1,
                    )]);
                }
            }
        }
        let events = self.service.submit_authorized(command, permit);
        for event in &events {
            match event {
                HostEvent::ChatAccepted { run_id, .. } => {
                    if let Some(chat) = &submission {
                        self.remember_submission(chat, run_id, provenance);
                    }
                }
                HostEvent::UserAnswerSaved {
                    question_id,
                    newly_saved: true,
                } => {
                    self.receipts.record_answer(question_id, provenance);
                    self.projection.note_answer(question_id, provenance);
                }
                _ => {}
            }
        }
        if let Err(error) = self.projection.refresh() {
            tracing::warn!(code = ?error.code, "host projection refresh failed");
        }
        Executed::Events(events)
    }

    /// Remember an accepted chat in memory and in storage. A storage failure does
    /// not undo the acceptance; it only means a resend after a restart is refused
    /// as unverifiable instead of being answered with the original run.
    fn remember_submission(&mut self, chat: &ChatInput, run_id: &str, provenance: &Provenance) {
        let fingerprint = chat_fingerprint(chat);
        let stored =
            self.service
                .runtime()
                .record_host_chat_submission(&storage::HostChatSubmission {
                    submission_id: chat.submission_id.clone(),
                    fingerprint: fingerprint.clone(),
                    run_id: run_id.to_owned(),
                    provenance_json: serde_json::to_string(provenance).unwrap_or_default(),
                    accepted_at_ns: storage::system_time_to_ns(std::time::SystemTime::now())
                        .unwrap_or_default(),
                });
        if let Err(error) = stored {
            tracing::warn!(%error, submission = %chat.submission_id, "could not persist chat submission");
        }
        self.receipts.record_submission(
            &chat.submission_id,
            SubmissionRecord {
                fingerprint,
                run_id: Some(run_id.to_owned()),
                sent_by: provenance.clone(),
            },
        );
    }

    /// What this host knows about a submission id, from storage and memory.
    ///
    /// Storage is authoritative: a row whose run was deleted is `Deleted` and a live
    /// row is compared by fingerprint. Memory covers what storage does not have: no run
    /// store at all, or a fingerprint that could not be stored. A memory entry whose
    /// input receipt is gone from a store that has receipts was reclaimed with its
    /// run, so it counts as deleted. The runtime's input receipt alone records only
    /// the words, which cannot prove two messages are the same.
    fn submission_status(&self, id: &str) -> Result<Seen, storage::StorageError> {
        let runtime = self.service.runtime();
        let ids = [id.to_owned()];
        match runtime.host_chat_submissions(&ids)?.into_iter().next() {
            Some(storage::HostChatSubmissionState::Deleted(_)) => return Ok(Seen::Deleted),
            Some(storage::HostChatSubmissionState::Live(stored)) => {
                return Ok(Seen::Known {
                    fingerprint: stored.fingerprint,
                    run_id: Some(stored.run_id),
                    sent_by: serde_json::from_str(&stored.provenance_json).ok(),
                });
            }
            None => {}
        }
        let has_receipt = !runtime.host_input_receipts(&ids)?.is_empty();
        Ok(match self.receipts.submission(id) {
            Some(record) if !runtime.has_run_store() || has_receipt => Seen::Known {
                fingerprint: record.fingerprint.clone(),
                run_id: record.run_id.clone(),
                sent_by: Some(record.sent_by.clone()),
            },
            Some(_) => Seen::Deleted,
            None if has_receipt => Seen::Unverifiable,
            None => Seen::New,
        })
    }

    /// The earlier outcome of a chat's submission id: `Ok(Some(run))` for the same
    /// message again, `Err` for an id that cannot be applied, `Ok(None)` for a new
    /// submission.
    ///
    /// "The same message" is the digest of every field of the message
    /// ([`chat_fingerprint`]): thread, project, composer role, text, skills, images,
    /// model preference and fork seed. An id whose run was deleted, or that has only
    /// the runtime's input receipt (accepted before fingerprints were stored, or while
    /// storing one failed), cannot be answered and is refused.
    fn earlier_submission(
        &mut self,
        chat: &ChatInput,
    ) -> Result<Option<Option<String>>, (RejectKind, String)> {
        let id = chat.submission_id.as_str();
        let status = self.submission_status(id).map_err(|error| {
            (
                RejectKind::Unavailable,
                format!("could not read earlier submissions: {error}"),
            )
        })?;
        match status {
            Seen::New => Ok(None),
            Seen::Known {
                fingerprint,
                run_id,
                ..
            } => {
                if fingerprint == chat_fingerprint(chat) {
                    Ok(Some(run_id))
                } else {
                    Err((
                        RejectKind::Conflict,
                        format!(
                            "submission `{}` was already used for a different message",
                            id.escape_debug()
                        ),
                    ))
                }
            }
            Seen::Deleted => {
                self.receipts.forget_submission(id);
                Err((
                    RejectKind::Conflict,
                    format!(
                        "submission `{}` belonged to a conversation that was deleted",
                        id.escape_debug()
                    ),
                ))
            }
            Seen::Unverifiable => Err((
                RejectKind::Conflict,
                format!(
                    "submission `{}` was accepted without a stored fingerprint; it cannot be \
                     checked against this message, so it is not applied again",
                    id.escape_debug()
                ),
            )),
        }
    }

    fn send_chat(
        &mut self,
        send: &SendChat,
        provenance: &Provenance,
    ) -> Result<CommandReply, WorkbenchError> {
        let command = HostCommand::SendChat(self.chat_input(send)?);
        let permit = self.service.authorize(&command).map_err(refusal_error)?;
        match self.execute(command, permit, provenance) {
            Executed::Duplicate { run_id, .. } => Ok(CommandReply {
                disposition: CommandDisposition::AlreadyApplied,
                submission_id: Some(send.submission_id.clone()),
                run_id: run_id.and_then(|run| workbench_protocol::ids::RunId::new(run).ok()),
                ..CommandReply::accepted()
            }),
            Executed::Events(events) => reply_from_events(events, Some(send.submission_id.clone())),
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
        let images = self.conversation.images(send)?;
        Ok(ChatInput {
            submission_id: send.submission_id.as_str().to_owned(),
            project_root: Some(self.project_root(&send.project)?),
            composer_role: composer_role(send.composer_role),
            images,
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

    fn goal_input(&self, goal: &SubmitGoal) -> Result<GoalInput, WorkbenchError> {
        match &goal.delegation {
            None | Some(Delegation::Direct) | Some(Delegation::Team { .. }) => {}
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
            delegation_value: match &goal.delegation {
                Some(Delegation::Team { value }) => Some(value.clone()),
                _ => None,
            },
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
                submission_id: Some(withdraw.submission_id.clone()),
                run_id: Some(withdraw.run.clone()),
                ..CommandReply::accepted()
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

fn unsupported(message: &str) -> WorkbenchError {
    reject_error(RejectKind::Unsupported, message)
}

/// The error for an authorisation refusal the service reported as an event.
fn refusal_error(refusal: HostEvent) -> WorkbenchError {
    match reply_from_events(vec![refusal], None) {
        Err(error) => error,
        Ok(_) => reject_error(RejectKind::Failed, "authorisation refused"),
    }
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
        RejectKind::Unavailable => (ErrorCode::StorageUnavailable, RetryAdvice::SameRequest),
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
        submission_id,
        ..CommandReply::accepted()
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
                outcome: NoticeOutcome::NotApplied,
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

fn approval_error(e: sandbox::approval::ApprovalError) -> WorkbenchError {
    use sandbox::approval::ApprovalError;
    reject_error(
        match e {
            ApprovalError::AlreadyDecided(_) => RejectKind::AlreadyResolved,
            ApprovalError::Unknown | ApprovalError::Expired | ApprovalError::Cancelled => {
                RejectKind::NotFound
            }
            ApprovalError::WrongEpoch | ApprovalError::RevisionMismatch { .. } => {
                RejectKind::Conflict
            }
            ApprovalError::InputTooLarge { .. } => RejectKind::Invalid,
        },
        &e.to_string(),
    )
}
