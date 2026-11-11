//! Conversation bootstrap and bounded, validated image ingress. The host owns
//! both the bytes and the authority; clients cannot nominate a file to read.
use std::{collections::BTreeMap, io::Cursor, path::PathBuf};

use base64::{Engine, engine::general_purpose::STANDARD};
use workbench_protocol::{
    commands::{CommandDisposition, CommandReply, ImageRef, SendChat},
    conversation::{ConversationCommand, ConversationResult},
    envelope::Provenance,
    errors::{ErrorCode, RetryAdvice, WorkbenchError},
    ids::{AttachmentId, ProjectId, ThreadId},
};

use super::CommandHost;

const MAX_ATTACHMENTS: usize = 32;
const MAX_ATTACHMENT_BYTES: usize = 8 * 1024 * 1024;

struct Attachment {
    project: ProjectId,
    thread: ThreadId,
    image: runtime::DelegateImage,
}

#[derive(Default)]
pub(super) struct ConversationState {
    attachments: BTreeMap<String, Attachment>,
    /// Only the composition root supplies these paths, never a wire request.
    legacy_import: Option<(PathBuf, PathBuf)>,
}

fn error(code: ErrorCode, message: &str) -> WorkbenchError {
    WorkbenchError::new(code, message, RetryAdvice::Never)
}

impl ConversationState {
    pub(super) fn images(
        &self,
        send: &SendChat,
    ) -> Result<Vec<runtime::DelegateImage>, WorkbenchError> {
        send.images
            .iter()
            .map(|reference| {
                let attachment = self
                    .attachments
                    .get(reference.attachment_id.as_str())
                    .ok_or_else(|| {
                        error(
                            ErrorCode::NotFound,
                            "attachment is unknown in this core epoch",
                        )
                    })?;
                if attachment.project != send.project
                    || attachment.thread != send.thread
                    || attachment.image.media_type != reference.mime_type
                {
                    return Err(error(
                        ErrorCode::PermissionDenied,
                        "attachment scope or media type mismatch",
                    ));
                }
                Ok(attachment.image.clone())
            })
            .collect()
    }

    fn upload(
        &mut self,
        project: ProjectId,
        thread: ThreadId,
        mime: String,
        data: String,
    ) -> Result<ImageRef, WorkbenchError> {
        let size: usize = self.attachments.values().map(|a| a.image.data.len()).sum();
        if self.attachments.len() >= MAX_ATTACHMENTS || size + data.len() > MAX_ATTACHMENT_BYTES {
            return Err(error(
                ErrorCode::LimitExceeded,
                "pending attachment capacity reached; discard unused attachments",
            ));
        }
        let bytes = STANDARD.decode(&data).map_err(|_| {
            error(
                ErrorCode::InvalidRequest,
                "image data must be canonical base64",
            )
        })?;
        if STANDARD.encode(&bytes) != data {
            return Err(error(
                ErrorCode::InvalidRequest,
                "image data must be canonical base64",
            ));
        }
        let format = image::guess_format(&bytes)
            .map_err(|_| error(ErrorCode::InvalidRequest, "unrecognised image format"))?;
        if !matches!(
            format,
            image::ImageFormat::Png
                | image::ImageFormat::Jpeg
                | image::ImageFormat::Gif
                | image::ImageFormat::WebP
        ) || format.to_mime_type() != mime
        {
            return Err(error(
                ErrorCode::InvalidRequest,
                "image content and supported media type must match",
            ));
        }
        let mut reader = image::ImageReader::with_format(Cursor::new(&bytes), format);
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(4096);
        limits.max_image_height = Some(4096);
        limits.max_alloc = Some(64 * 1024 * 1024);
        reader.limits(limits);
        reader.decode().map_err(|_| {
            error(
                ErrorCode::InvalidRequest,
                "image is corrupt or exceeds decoding limits",
            )
        })?;
        let attachment_id =
            AttachmentId::new(uuid::Uuid::new_v4().to_string()).expect("uuid is a valid id");
        self.attachments.insert(
            attachment_id.as_str().into(),
            Attachment {
                project,
                thread,
                image: runtime::DelegateImage {
                    media_type: mime.clone(),
                    data,
                },
            },
        );
        Ok(ImageRef {
            attachment_id,
            mime_type: mime,
        })
    }
}

impl CommandHost {
    /// Enables the narrow opt-in import path. Access is re-read from the host's
    /// sidebar declarations at each import; neither the wire nor the repository
    /// can assert trust. This does not grant permission to claim live owners.
    pub fn with_legacy_import(mut self, ownership_root: PathBuf, access_file: PathBuf) -> Self {
        self.conversation.legacy_import = Some((ownership_root, access_file));
        self
    }

    pub(super) fn conversation_command(
        &mut self,
        input: ConversationCommand,
        provenance: &Provenance,
    ) -> Result<CommandReply, WorkbenchError> {
        let result = match input {
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
                // Validate attachments before creating even an owner row.
                self.chat_input(&chat)?;
                self.reserve_new_chat_thread(&chat)?;
                return self.send_chat(&chat, provenance);
            }
            ConversationCommand::CreateGoal(goal) => {
                self.goal_input(&goal)?;
                self.reserve_new_thread(&goal.project, &goal.thread)?;
                return self.handle_command(
                    workbench_protocol::commands::Command::SubmitGoal(goal),
                    provenance.clone(),
                );
            }
            ConversationCommand::PrepareExistingThread { project, thread } => {
                self.prepare_existing(&project, &thread)?;
                ConversationResult::ExistingThreadPrepared { thread }
            }
            ConversationCommand::UploadImage {
                project,
                thread,
                mime_type,
                data,
            } => {
                self.project_root(&project)?;
                self.check_thread_not_deleted(&thread)?;
                ConversationResult::ImageUploaded(
                    self.conversation.upload(project, thread, mime_type, data)?,
                )
            }
            ConversationCommand::DiscardAttachment { attachment_id } => {
                self.conversation.attachments.remove(attachment_id.as_str());
                ConversationResult::AttachmentDiscarded
            }
        };
        Ok(CommandReply {
            conversation: Some(Box::new(result)),
            lane_management: None,
            lane_close: None,
            disposition: CommandDisposition::Accepted,
            submission_id: None,
            run_id: None,
            thread_revision: None,
        })
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
