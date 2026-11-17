use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::mpsc::TryRecvError,
};
use workbench_protocol::commands::AttachmentRef;
use workspace_ui::ThreadId;

use super::WorkbenchState;
use crate::{
    attachments::{AttachmentIo, Intake},
    model::{
        commands::AttachmentUploadReceiver,
        composer::{AttachmentDraft, AttachmentUploadState},
        tasks::AgentRunSource,
    },
};

struct Upload {
    thread: ThreadId,
    project: PathBuf,
    draft_id: String,
    receiver: AttachmentUploadReceiver,
}

struct Uploaded {
    thread: ThreadId,
    project: PathBuf,
    draft_id: String,
    reference: AttachmentRef,
}

#[derive(Default)]
pub(super) struct AttachmentWork {
    pub io: AttachmentIo,
    pub queued: Vec<(ThreadId, Intake)>,
    imports: BTreeMap<String, ThreadId>,
    uploads: Vec<Upload>,
    uploaded: Vec<Uploaded>,
}

impl<S: AgentRunSource> WorkbenchState<S> {
    fn attachment_drafts_mut(&mut self, thread: &ThreadId) -> Option<&mut Vec<AttachmentDraft>> {
        if self.sidebar.active_thread.as_ref() == Some(thread) {
            Some(&mut self.composer.attachments)
        } else {
            self.composer_attachments.get_mut(thread)
        }
    }

    pub(super) fn poll_attachments(&mut self, ctx: &egui::Context) {
        if std::mem::take(&mut self.composer.pick_attachments)
            && let Some(thread) = self.sidebar.active_thread.clone()
        {
            self.attachment_work
                .queued
                .push((thread, Intake::ChooseFiles));
        }
        for (thread, intake) in std::mem::take(&mut self.attachment_work.queued) {
            match self
                .attachment_work
                .io
                .submit(&thread.to_string(), intake, ctx)
            {
                Ok(request) => {
                    self.attachment_work.imports.insert(request, thread);
                }
                Err(error) => self.push_notice(error),
            }
        }
        let results = self.attachment_work.io.poll().collect::<Vec<_>>();
        for result in results {
            let Some(thread) = self.attachment_work.imports.remove(&result.request_id) else {
                continue;
            };
            if thread.to_string() != result.thread || !self.attachment_thread_active(&thread) {
                continue;
            }
            match result.files {
                Ok(files) => {
                    for file in files {
                        if self.sidebar.active_thread.as_ref() == Some(&thread) {
                            if let Err(error) =
                                self.composer.add_attachment(AttachmentDraft::new(file))
                            {
                                self.push_notice(error);
                            }
                        } else {
                            let drafts =
                                self.composer_attachments.entry(thread.clone()).or_default();
                            let count = drafts.len();
                            let bytes = drafts
                                .iter()
                                .map(|draft| draft.file.bytes.len())
                                .sum::<usize>();
                            if count >= 32 || bytes + file.bytes.len() > 64 * 1024 * 1024 {
                                self.push_notice(
                                    "Attachment draft limit reached (32 files / 64 MiB)",
                                );
                            } else {
                                drafts.push(AttachmentDraft::new(file));
                            }
                        }
                    }
                }
                Err(error) => self.push_notice(error),
            }
        }
        let mut pending = Vec::new();
        if let Some(thread) = self.sidebar.active_thread.as_ref() {
            pending.extend(
                self.composer
                    .attachments
                    .iter()
                    .filter(|draft| draft.upload == AttachmentUploadState::Pending)
                    .map(|draft| (thread.clone(), draft.file.clone())),
            );
        }
        for (thread, drafts) in &self.composer_attachments {
            pending.extend(
                drafts
                    .iter()
                    .filter(|draft| draft.upload == AttachmentUploadState::Pending)
                    .map(|draft| (thread.clone(), draft.file.clone())),
            );
        }
        for (thread, file) in pending {
            let draft_id = file.draft_id.clone();
            let project = self
                .thread_project(&thread)
                .map(|project| project.repo_root.clone());
            let result = project
                .as_ref()
                .ok_or_else(|| "Select a project before uploading attachments".to_owned())
                .and_then(|project| {
                    self.sink
                        .upload_attachment(project, &thread.to_string(), file)
                });
            let state = match result {
                Ok(receiver) => {
                    self.attachment_work.uploads.push(Upload {
                        thread: thread.clone(),
                        project: project.expect("project checked"),
                        draft_id: draft_id.clone(),
                        receiver,
                    });
                    AttachmentUploadState::Uploading
                }
                Err(error) => AttachmentUploadState::Failed(error),
            };
            if let Some(draft) = self.attachment_drafts_mut(&thread).and_then(|drafts| {
                drafts
                    .iter_mut()
                    .find(|draft| draft.file.draft_id == draft_id)
            }) {
                draft.upload = state;
            }
        }
        for upload in std::mem::take(&mut self.attachment_work.uploads) {
            let result = match upload.receiver.try_recv() {
                Ok(result) => result,
                Err(TryRecvError::Empty) => {
                    self.attachment_work.uploads.push(upload);
                    continue;
                }
                Err(TryRecvError::Disconnected) => Err("Attachment upload worker stopped".into()),
            };
            let draft = self
                .attachment_drafts_mut(&upload.thread)
                .and_then(|drafts| {
                    drafts
                        .iter_mut()
                        .find(|draft| draft.file.draft_id == upload.draft_id)
                });
            match (draft, result) {
                (Some(draft), Ok(reference)) => {
                    draft.upload = AttachmentUploadState::Ready(reference.clone());
                    self.attachment_work.uploaded.push(Uploaded {
                        thread: upload.thread,
                        project: upload.project,
                        draft_id: upload.draft_id,
                        reference,
                    });
                }
                (Some(draft), Err(error)) => draft.upload = AttachmentUploadState::Failed(error),
                (None, Ok(reference)) => self.sink.discard_attachment(
                    &upload.project,
                    &upload.thread.to_string(),
                    &reference.id,
                ),
                (None, Err(_)) => {}
            }
        }
        let retained = self
            .composer
            .attachments
            .iter()
            .chain(self.composer_attachments.values().flatten())
            .chain(self.queued_attachments.values().flatten())
            .map(|draft| draft.file.draft_id.clone())
            .collect::<BTreeSet<_>>();
        for uploaded in std::mem::take(&mut self.attachment_work.uploaded) {
            if retained.contains(&uploaded.draft_id) {
                self.attachment_work.uploaded.push(uploaded);
            } else {
                self.sink.discard_attachment(
                    &uploaded.project,
                    &uploaded.thread.to_string(),
                    &uploaded.reference.id,
                );
            }
        }
        self.refresh_attachment_import_count();
        if !self.attachment_work.uploads.is_empty() {
            // Upload completion uses a channel, independent of paint frequency.
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
    }

    fn attachment_thread_active(&self, thread: &ThreadId) -> bool {
        self.sidebar
            .resolve_states()
            .get(thread)
            .is_some_and(|state| !state.archived)
    }

    pub(super) fn refresh_attachment_import_count(&mut self) {
        self.composer.attachment_imports = self
            .attachment_work
            .imports
            .values()
            .filter(|thread| self.sidebar.active_thread.as_ref() == Some(*thread))
            .count();
    }

    pub(super) fn forget_family_attachment_drafts(&mut self, family: &BTreeSet<ThreadId>) {
        self.attachment_work
            .imports
            .retain(|_, thread| !family.contains(thread));
        self.attachment_work
            .queued
            .retain(|(thread, _)| !family.contains(thread));
        self.composer_attachments
            .retain(|thread, _| !family.contains(thread));
        if self
            .sidebar
            .active_thread
            .as_ref()
            .is_some_and(|thread| family.contains(thread))
        {
            self.composer.attachments.clear();
            self.composer.pick_attachments = false;
        }
        let family_messages = self
            .history
            .iter()
            .filter(|message| {
                family
                    .iter()
                    .any(|thread| thread.to_string() == message.thread_id)
            })
            .map(|message| message.id.clone())
            .collect::<BTreeSet<_>>();
        self.queued_attachments
            .retain(|id, _| !family_messages.contains(id));
        self.refresh_attachment_import_count();
    }
}
