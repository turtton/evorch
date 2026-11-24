//! Metadata-only generic attachment cards. Loading, clipboard access and upload
//! happen in workers; corrupt images are valid files and never decoded here.
use crate::model::composer::{AttachmentUploadState, ComposerModel};
use crate::panes::attachment_widgets::{
    AttachmentRowStatus, AttachmentRowView, attach_files_button, attachment_rows, preparing,
};

pub(super) fn render(ui: &mut egui::Ui, model: &mut ComposerModel) {
    if attach_files_button(ui).clicked() {
        model.pick_attachments = true;
    }
    preparing(ui, model.attachment_imports);
    let rows: Vec<_> = model
        .attachments
        .iter()
        .map(|attachment| AttachmentRowView {
            key: &attachment.file.draft_id,
            filename: &attachment.file.filename,
            media_type: &attachment.file.media_type,
            bytes: attachment.file.bytes.len(),
            status: match &attachment.upload {
                AttachmentUploadState::Pending | AttachmentUploadState::Uploading => {
                    AttachmentRowStatus::Uploading
                }
                AttachmentUploadState::Ready(_) => AttachmentRowStatus::Ready,
                AttachmentUploadState::Failed(error) => AttachmentRowStatus::Failed(error),
            },
            removable: true,
        })
        .collect();
    if let Some(index) = attachment_rows(ui, &rows) {
        model.remove_attachment(index);
    }
}
