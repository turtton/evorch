//! Conversation bootstrap and bounded, validated image ingress. The host owns
//! both the bytes and the authority; clients cannot nominate a file to read.
use std::path::PathBuf;

use workbench_protocol::{
    commands::{CommandReply, CommandResult, ContinueChat},
    conversation::{AttachmentLimits, ConversationCommand, ConversationResult},
    envelope::Provenance,
    errors::{ErrorCode, RetryAdvice, WorkbenchError},
    ids::ThreadId,
};

use crate::commands::{ContinueInput, HostCommand};
use crate::ingress::{CommandHost, composer_role};
use workbench_protocol::namespaces::ExecutionCommand;

fn error(code: ErrorCode, message: &str) -> WorkbenchError {
    WorkbenchError::new(code, message, RetryAdvice::Never)
}

impl CommandHost {
    /// The conversation namespace: every operation reaches the runtime through
    /// the same admission checks, submission ledger and command service as every
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
                model_preference: _,
            }) => {
                return self.run_host_command(
                    HostCommand::ContinueChat(ContinueInput {
                        project_root: Some(self.project_root(&project)?),
                        thread_id: thread.as_str().to_owned(),
                        composer_role: composer_role(role),
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
                        (project, thread),
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
