//! The composer's attachment block, shared by the embedded workbench's composer
//! and the core client's: the picker button, the files being prepared, and one
//! row per file with its upload state and Remove. Metadata only; files are read
//! and uploaded elsewhere. The core client's composer uses the compact forms: a
//! round "+" beside the input and one chip per file.

use egui::{CornerRadius, Margin, RichText, Stroke, WidgetInfo, WidgetType};
use workbench_protocol::conversation::ATTACHMENT_FILE_BYTES;

use crate::theme::icons;
use crate::theme::tokens::*;
use crate::theme::widgets::{accessible, ghost, ghost_icon_button, icon_text, spinner_with};

/// The widest a chip's file name gets before it is cut short.
const CHIP_NAME_WIDTH: f32 = 180.0;

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

/// The round "+" that opens the file picker. Most files arrive by paste or drop,
/// which the tooltip says; it also shows while the button has keyboard focus.
pub fn attach_plus_button(ui: &mut egui::Ui) -> egui::Response {
    let side = ROW_DENSE - SP_1;
    let response = ui
        .scope(|ui| {
            // Little enough padding that the glyph fits a circle as tall as Send, and an
            // outline that shows the circle at rest.
            ui.spacing_mut().button_padding = egui::vec2(SP_1, SP_1);
            let idle = &mut ui.visuals_mut().widgets.inactive;
            idle.weak_bg_fill = palette().INPUT;
            idle.bg_stroke = Stroke::new(1.0, palette().BORDER);
            ui.add(
                egui::Button::new(icon_text(icons::PLUS))
                    .corner_radius(CornerRadius::same(R_PILL))
                    .min_size(egui::vec2(side, side)),
            )
        })
        .inner;
    accessible(&response, "Attach files");
    let hint = format!(
        "Attach files (up to {} MiB each). You can also paste or drop them into the message.",
        ATTACHMENT_FILE_BYTES / (1024 * 1024)
    );
    if response.enabled() && response.has_focus() {
        response.show_tooltip_text(hint);
        response
    } else {
        response.on_hover_text(hint)
    }
}

/// One compact chip per file, laid out by the caller (a wrapping row): its name,
/// upload state and Remove. Returns the index of the chip whose Remove was pressed.
pub fn attachment_chips(ui: &mut egui::Ui, rows: &[AttachmentRowView<'_>]) -> Option<usize> {
    let mut remove = None;
    for (index, row) in rows.iter().enumerate() {
        // A chip is a nested ui, which a wrapping row cannot measure before placing
        // it, so it starts the next row when its width in the last frame does not fit.
        let width_id = ui.id().with(("attachment-chip-width", row.key));
        let width = ui.data(|data| data.get_temp::<f32>(width_id));
        let started = ui.cursor().min.x > ui.max_rect().min.x + 0.5;
        if started && width.is_some_and(|width| width > ui.available_rect_before_wrap().width()) {
            ui.end_row();
        }
        let chip = ui.push_id(row.key, |ui| attachment_chip(ui, row)).inner;
        ui.data_mut(|data| data.insert_temp(width_id, chip.response.rect.width()));
        if chip.inner {
            remove = Some(index);
        }
    }
    remove
}

fn attachment_chip(ui: &mut egui::Ui, row: &AttachmentRowView<'_>) -> egui::InnerResponse<bool> {
    let p = palette();
    let failed = matches!(row.status, AttachmentRowStatus::Failed(_));
    egui::Frame::new()
        .fill(p.INPUT)
        .stroke(Stroke::new(1.0, if failed { p.WARNING } else { p.BORDER }))
        .corner_radius(CornerRadius::same(R_PILL))
        .inner_margin(Margin {
            left: SP_2 as i8,
            right: SP_1 as i8,
            top: 2,
            bottom: 2,
        })
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = SP_1;
                let icon = if row.media_type.starts_with("image/") {
                    icons::IMAGE
                } else {
                    icons::FILE
                };
                ui.label(RichText::new(icon).color(p.TEXT_MUTED));
                let name = ui
                    .scope(|ui| {
                        ui.set_max_width(CHIP_NAME_WIDTH);
                        ui.add(
                            egui::Label::new(RichText::new(row.filename).size(FONT_SMALL))
                                .truncate(),
                        )
                    })
                    .inner;
                name.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, row.filename));
                name.on_hover_text(format!(
                    "{}\n{} · {} bytes",
                    row.filename, row.media_type, row.bytes
                ));
                match row.status {
                    AttachmentRowStatus::Uploading => {
                        let spinner = spinner_with(ui, FONT_SMALL, None);
                        spinner.widget_info(|| {
                            WidgetInfo::labeled(WidgetType::ProgressIndicator, true, "Uploading…")
                        });
                        spinner.on_hover_text("Uploading…");
                    }
                    AttachmentRowStatus::Ready => {
                        let ready = ui.label(RichText::new(icons::CHECK).color(p.SUCCESS));
                        ready.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, "Ready"));
                        ready.on_hover_text("Uploaded");
                    }
                    AttachmentRowStatus::Failed(error) => {
                        ui.label(RichText::new(error).size(FONT_SMALL).color(p.WARNING_FG));
                    }
                }
                let side = FONT_ICON;
                let response = ui.add_enabled(
                    row.removable,
                    ghost(RichText::new(icons::X).size(FONT_SMALL))
                        .corner_radius(CornerRadius::same(R_PILL))
                        .min_size(egui::vec2(side, side)),
                );
                accessible(&response, "Remove");
                response
                    .on_hover_text(format!("Remove {}", row.filename))
                    .clicked()
            })
            .inner
        })
}

pub fn retry_upload_button(ui: &mut egui::Ui) -> egui::Response {
    ghost_icon_button(ui, icons::ARROW_CLOCKWISE, "Retry upload")
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
