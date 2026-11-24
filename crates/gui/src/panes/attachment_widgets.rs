//! The composer's attachment block, shared by the embedded workbench's composer
//! and the core client's: the picker button, the files being prepared, and one
//! row per file with its upload state and Remove. Metadata only; files are read
//! and uploaded elsewhere.

use crate::theme::tokens::palette;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachmentRowStatus<'a> {
    Uploading,
    Ready,
    Failed(&'a str),
}

pub struct AttachmentRowView<'a> {
    /// Keeps each row's widget ids stable while rows are removed.
    pub key: &'a str,
    pub filename: &'a str,
    pub media_type: &'a str,
    pub bytes: usize,
    pub status: AttachmentRowStatus<'a>,
    pub removable: bool,
}

pub fn attach_files_button(ui: &mut egui::Ui) -> egui::Response {
    ui.button("Attach files…")
        .on_hover_text("Choose ordinary files (up to 8 MiB each)")
}

pub fn preparing(ui: &mut egui::Ui, count: usize) {
    if count == 0 {
        return;
    }
    ui.horizontal(|ui| {
        ui.spinner();
        ui.label(format!("Preparing {count} attachment(s)…"));
    });
}

/// The index of the row whose Remove was pressed.
pub fn attachment_rows(ui: &mut egui::Ui, rows: &[AttachmentRowView<'_>]) -> Option<usize> {
    let mut remove = None;
    for (index, row) in rows.iter().enumerate() {
        ui.push_id(row.key, |ui| {
            ui.horizontal(|ui| {
                ui.label(row.filename);
                ui.small(format!("{} · {} bytes", row.media_type, row.bytes));
                match row.status {
                    AttachmentRowStatus::Uploading => {
                        ui.spinner();
                        ui.small("Uploading…");
                    }
                    AttachmentRowStatus::Ready => {
                        ui.small("Ready");
                    }
                    AttachmentRowStatus::Failed(error) => {
                        ui.colored_label(palette().WARNING_FG, error);
                    }
                }
                if ui
                    .add_enabled(row.removable, egui::Button::new("Remove").small())
                    .clicked()
                {
                    remove = Some(index);
                }
            });
        });
    }
    remove
}
