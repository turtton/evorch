//! Conversation bootstrap and bounded, validated image ingress. The host owns
//! both the bytes and the authority; clients cannot nominate a file to read.
use std::path::PathBuf;

use workbench_protocol::{
    commands::{CommandReply, CommandResult, ContinueChat},
    conversation::{AttachmentLimits, ConversationCommand, ConversationResult},
    envelope::Provenance,
    errors::{ErrorCode, RetryAdvice, WorkbenchError},
    ids::{ProjectId, ThreadId},
};

use crate::commands::{ContinueInput, HostCommand};
use crate::ingress::{CommandHost, composer_role, model_preference};
use workbench_protocol::namespaces::ExecutionCommand;

#[derive(Default)]
pub(in crate::ingress) struct ConversationState {
    legacy_import: Option<(PathBuf, PathBuf)>,
}
fn error(code: ErrorCode, message: &str) -> WorkbenchError {
    WorkbenchError::new(code, message, RetryAdvice::Never)
}

impl CommandHost {
    /// Enables the narrow opt-in import path. Access is re-read from the host's
    /// sidebar declarations at each import; neither the wire nor the repository
    /// can assert trust. This does not grant permission to claim live owners.
    pub fn with_legacy_import(mut self, ownership_root: PathBuf, access_file: PathBuf) -> Self {
        self.conversation.legacy_import = Some((ownership_root, access_file));
        self
    }

    /// The conversation namespace: every operation reaches the runtime through
    /// the same ownership check, submission ledger and command service as every
    /// other namespace; this adapter only translates the wire form.
    pub(in crate::ingress) fn conversation_command(
        &mut self,
        input: ConversationCommand,
        provenance: &Provenance,
        restart: crate::bootstrap::restart::HostCommandPermit,
    ) -> Result<CommandReply, WorkbenchError> {
        let result = match input {
            ConversationCommand::SendChat(send) => return self.send_chat(&send, provenance),
            ConversationCommand::WithdrawFollowUp(withdraw) => {
                return self.withdraw_follow_up(&withdraw);
            }
            ConversationCommand::ContinueChat(ContinueChat {
                project,
                thread,
                composer_role: role,
                model_preference: preference,
            }) => {
                return self.run_host_command(
                    HostCommand::ContinueChat(ContinueInput {
                        project_root: Some(self.project_root(&project)?),
                        thread_id: thread.as_str().to_owned(),
                        composer_role: composer_role(role),
                        model_preference: preference.as_ref().map(model_preference),
                        // Automatic continuations are the core's own decision.
                        automatic: false,
                    }),
                    provenance,
                );
            }
            ConversationCommand::CompactChat(target) => {
                return self.run_host_command_tracked(
                    HostCommand::CompactChat {
                        thread_id: target.thread.as_str().to_owned(),
                    },
                    provenance,
                    Some(restart),
                );
            }
            ConversationCommand::DeliverFollowUpsNextTurn(target) => {
                return self.run_host_command(
                    HostCommand::DeliverFollowUpsNextTurn {
                        thread_id: target.thread.as_str().to_owned(),
                    },
                    provenance,
                );
            }
            ConversationCommand::ResolveProject { root } => {
                let path = PathBuf::from(root);
                if !path.is_absolute() {
                    return Err(error(
                        ErrorCode::InvalidRequest,
                        "project root must be absolute",
                    ));
                }
                let root = std::fs::canonicalize(path)
                    .map_err(|_| error(ErrorCode::NotFound, "project root is unavailable"))?;
                let project = self
                    .projects
                    .entries()
                    .into_iter()
                    .find(|(_, path)| std::fs::canonicalize(path).is_ok_and(|path| path == root))
                    .map(|(id, _)| id)
                    .ok_or_else(|| {
                        error(
                            ErrorCode::NotFound,
                            "project is not registered with this core",
                        )
                    })?;
                ConversationResult::ProjectResolved { project }
            }
            ConversationCommand::CreateExecution(chat) => {
                return self.send_chat(&chat, provenance);
            }
            ConversationCommand::CreateGoal(goal) => {
                self.goal_input(&goal)?;
                self.reserve_new_thread(&goal.project, &goal.thread, &goal.objective, true, None)?;
                return self.handle_command(
                    workbench_protocol::commands::Command::Execution(ExecutionCommand::SubmitGoal(
                        goal,
                    )),
                    provenance.clone(),
                );
            }
            ConversationCommand::PrepareExistingThread { project, thread } => {
                self.prepare_existing(&project, &thread)?;
                ConversationResult::ExistingThreadPrepared { thread }
            }
            ConversationCommand::BeginAttachment {
                project,
                thread,
                client_upload_id,
                filename,
                media_type_hint,
                byte_length,
            } => {
                self.project_root(&project)?;
                self.check_thread_not_deleted(&thread)?;
                self.check_attachment_thread_active(thread.as_str())?;
                let upload_id = self.with_attachment_service(|service, host| {
                    service.begin_checked(
                        project,
                        thread,
                        client_upload_id,
                        filename,
                        media_type_hint,
                        byte_length,
                        |thread| host.check_attachment_thread_active(thread),
                    )
                })?;
                ConversationResult::AttachmentBegun {
                    upload_id,
                    limits: AttachmentLimits {
                        file_bytes: crate::attachments::FILE_BYTES,
                        chunk_bytes: crate::attachments::CHUNK_BYTES as u64,
                        thread_pending_count: crate::attachments::THREAD_PENDING_COUNT as u32,
                        thread_pending_bytes: crate::attachments::THREAD_PENDING_BYTES,
                        store_pending_count: crate::attachments::STORE_PENDING_COUNT as u32,
                        store_pending_bytes: crate::attachments::STORE_PENDING_BYTES,
                        thread_sent_bytes: crate::attachments::THREAD_SENT_BYTES,
                    },
                }
            }
            ConversationCommand::AppendAttachment {
                upload_id,
                offset,
                data_base64,
            } => ConversationResult::AttachmentAppended {
                next_offset: self.with_attachment_service(|service, host| {
                    service.append_base64_checked(&upload_id, offset, &data_base64, |thread| {
                        host.check_attachment_thread_active(thread)
                    })
                })?,
            },
            ConversationCommand::FinishAttachment { upload_id } => {
                ConversationResult::AttachmentFinished(self.with_attachment_service(
                    |service, host| {
                        service.finish_checked(&upload_id, |thread| {
                            host.check_attachment_thread_active(thread)
                        })
                    },
                )?)
            }
            ConversationCommand::DiscardAttachment { attachment_id } => {
                self.attachment_service()?.discard(&attachment_id)?;
                ConversationResult::AttachmentDiscarded
            }
        };
        Ok(CommandReply::with_result(CommandResult::Conversation(
            result,
        )))
    }

    fn check_thread_not_deleted(&self, thread: &ThreadId) -> Result<(), WorkbenchError> {
        let deleted = self
            .service
            .runtime()
            .with_storage_reader(|db| Ok(db.tombstones()?.contains_thread(thread.as_str())))
            .map_err(|_| error(ErrorCode::Busy, "could not check conversation tombstones"))?;
        if deleted == Some(true) {
            return Err(error(ErrorCode::NotFound, "conversation was deleted"));
        }
        Ok(())
    }

    fn prepare_existing(
        &mut self,
        project: &ProjectId,
        thread: &ThreadId,
    ) -> Result<(), WorkbenchError> {
        let root = std::fs::canonicalize(self.project_root(project)?)
            .map_err(|_| error(ErrorCode::NotFound, "project root is unavailable"))?;
        let (ownership_root, access_file) =
            self.conversation.legacy_import.as_ref().ok_or_else(|| {
                error(
                    ErrorCode::UnsupportedCapability,
                    "legacy conversation import is disabled",
                )
            })?;
        #[derive(serde::Deserialize)]
        struct Sidebar {
            projects: Vec<Project>,
        }
        #[derive(serde::Deserialize)]
        struct Project {
            id: ProjectId,
            repo_root: PathBuf,
            #[serde(flatten)]
            access: crate::bootstrap::components::ProjectAccess,
        }
        let declarations: Sidebar = std::fs::read(access_file)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .ok_or_else(|| {
                error(
                    ErrorCode::PermissionDenied,
                    "host project trust is unavailable",
                )
            })?;
        let access = declarations
            .projects
            .into_iter()
            .find(|p| {
                p.id == *project
                    && std::fs::canonicalize(&p.repo_root).is_ok_and(|path| path == root)
                    && matches!(
                        p.access.trust,
                        crate::bootstrap::components::Trust::Approved
                    )
            })
            .map(|p| p.access)
            .ok_or_else(|| {
                error(
                    ErrorCode::PermissionDenied,
                    "project is not approved by the host",
                )
            })?;
        // A store is mandatory for import: absence is not evidence of an empty
        // thread, and a client-supplied project must match the persisted binding.
        let stored = self
            .service
            .runtime()
            .with_storage_reader(|db| {
                if db.tombstones()?.contains_thread(thread.as_str()) {
                    return Ok(None);
                }
                db.thread(thread.as_str())
            })
            .map_err(|_| error(ErrorCode::Busy, "could not validate stored conversation"))?
            .flatten()
            .ok_or_else(|| error(ErrorCode::NotFound, "conversation is missing or deleted"))?;
        let stored_root = stored
            .project_root
            .and_then(|path| std::fs::canonicalize(path).ok());
        if stored_root.as_ref() != Some(&root) {
            return Err(error(
                ErrorCode::PermissionDenied,
                "conversation belongs to another project",
            ));
        }
        let run = self
            .service
            .runtime()
            .latest_chat_run(thread.as_str())
            .map_err(|_| error(ErrorCode::Conflict, "conversation cannot be restored"))?
            .ok_or_else(|| error(ErrorCode::NotFound, "conversation has no restorable root"))?;
        let owner = self.service.ownership().ok_or_else(|| {
            error(
                ErrorCode::UnsupportedCapability,
                "ownership service is unavailable",
            )
        })?;
        let expected = owner.attach(thread.as_str()).map_err(|_| {
            error(
                ErrorCode::PermissionDenied,
                "conversation owner is unavailable",
            )
        })?;
        if expected.lease.owner_id == owner.owner_id()
            && self.service.bindings.root_of(thread.as_str()).is_some()
        {
            owner.owned_permit(thread.as_str()).map_err(|_| {
                error(
                    ErrorCode::PermissionDenied,
                    "conversation ownership is fenced",
                )
            })?;
            self.check_thread_not_deleted(thread)?;
            access.apply(self.service.runtime(), root);
            return Ok(());
        }
        let permit = if expected.lease.owner_id == owner.owner_id() {
            owner.owned_permit(thread.as_str())
        } else {
            // Released alone is not proof that the legacy process has exited.
            // A responsive OR uncertain endpoint is protected. The canonical
            // claim below rechecks the generation and tombstone guard.
            if expected.state != runtime::ownership::OwnerState::Released
                || runtime::ownership::ipc::owner_may_be_live(
                    ownership_root,
                    &expected.lease.owner_id,
                )
            {
                return Err(error(
                    ErrorCode::PermissionDenied,
                    "legacy owner must be released and offline",
                ));
            }
            owner.claim(&expected)
        }
        .map_err(|_| {
            error(
                ErrorCode::PermissionDenied,
                "conversation claim was refused",
            )
        })?;
        self.check_thread_not_deleted(thread)?;
        access.apply(self.service.runtime(), root.clone());
        self.service
            .bindings
            .chat_runs
            .insert(thread.as_str().into(), run);
        self.service
            .bindings
            .chat_permits
            .insert(thread.as_str().into(), permit);
        self.service
            .bindings
            .thread_roots
            .insert(thread.as_str().into(), root);
        self.service.publish_lane_bindings();
        Ok(())
    }
}

impl CommandHost {
    /// Input receipts and committed history are read from the projection.
    pub(in crate::ingress) fn conversation_query(
        &self,
        query: &workbench_protocol::queries::Query,
        _namespace_query: &workbench_protocol::namespaces::ConversationQuery,
    ) -> Result<workbench_protocol::queries::QueryReply, WorkbenchError> {
        self.projection.query(query)
    }
}
