//! Metadata-only generic attachment cards. Loading, clipboard access and upload
//! happen in workers; corrupt images are valid files and never decoded here.
use crate::model::composer::{AttachmentUploadState, ComposerModel};

pub(super) fn render(ui: &mut egui::Ui, model: &mut ComposerModel) {
    if ui
        .button("Attach files…")
        .on_hover_text("Choose ordinary files (up to 8 MiB each)")
        .clicked()
    {
        model.pick_attachments = true;
    }
    if model.attachment_imports != 0 {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(format!(
                "Preparing {} attachment(s)…",
                model.attachment_imports
            ));
        });
    }
    let mut remove = None;
    for (index, attachment) in model.attachments.iter().enumerate() {
        ui.push_id(&attachment.file.draft_id, |ui| {
            ui.horizontal(|ui| {
                ui.label(&attachment.file.filename);
                ui.small(format!(
                    "{} · {} bytes",
                    attachment.file.media_type,
                    attachment.file.bytes.len()
                ));
                match &attachment.upload {
                    AttachmentUploadState::Pending | AttachmentUploadState::Uploading => {
                        ui.spinner();
                        ui.small("Uploading…");
                    }
                    AttachmentUploadState::Ready(_) => {
                        ui.small("Ready");
                    }
                    AttachmentUploadState::Failed(error) => {
                        ui.colored_label(crate::theme::tokens::palette().WARNING_FG, error);
                    }
                }
                if ui.small_button("Remove").clicked() {
                    remove = Some(index);
                }
            });
        });
    }
    if let Some(index) = remove {
        model.remove_attachment(index);
    }
}
