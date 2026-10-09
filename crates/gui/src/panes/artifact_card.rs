//! Conversation cards for artifacts presented by the conversation owner.
//!
//! Opening goes through a request queue instead of `OpenUrl`: local `file://`
//! links are consumed by the in-app file viewer, which would show HTML source.

use std::path::{Path, PathBuf};

use egui::{RichText, Ui};
use event_bus::{ArtifactPresentation, PresentedArtifact};

use crate::theme::icons;
use crate::theme::text::medium;
use crate::theme::tokens::{FONT_SMALL, SP_1, SP_2, palette};
use crate::theme::widgets::{ghost_icon_button, soft_frame};

const PREVIEW_MAX_WIDTH: f32 = 640.0;
const PREVIEW_MAX_HEIGHT: f32 = 480.0;

fn open_requests_id() -> egui::Id {
    egui::Id::new("artifact-open-requests")
}

/// Ask the host to open a stored artifact with the desktop's default application.
pub fn request_open(ctx: &egui::Context, path: PathBuf) {
    ctx.data_mut(|data| {
        data.get_temp_mut_or_default::<Vec<PathBuf>>(open_requests_id())
            .push(path);
    });
}

/// Drain the open requests issued since the previous call.
pub fn take_open_requests(ctx: &egui::Context) -> Vec<PathBuf> {
    ctx.data_mut(|data| {
        data.remove_temp::<Vec<PathBuf>>(open_requests_id())
            .unwrap_or_default()
    })
}

/// One-line text form used for accessibility and plain transcript labels.
pub fn summary(presentation: &ArtifactPresentation) -> String {
    let titles = presentation
        .artifacts
        .iter()
        .map(|artifact| artifact.title.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    match &presentation.title {
        Some(title) => format!("Artifacts: {title} ({titles})"),
        None => format!("Artifacts: {titles}"),
    }
}

pub fn show(ui: &mut Ui, id: egui::Id, presentation: &ArtifactPresentation) {
    soft_frame(palette().SURFACE).show(ui, |ui| {
        ui.set_width(ui.available_width());
        if let Some(title) = &presentation.title {
            ui.label(medium(icons::with_icon(icons::IMAGES, title)).color(palette().TEXT));
        }
        if let Some(caption) = &presentation.caption {
            ui.label(RichText::new(caption).color(palette().TEXT_MUTED));
        }
        for (index, artifact) in presentation.artifacts.iter().enumerate() {
            if index > 0 || presentation.title.is_some() || presentation.caption.is_some() {
                ui.add_space(SP_2);
            }
            ui.push_id(id.with(index), |ui| artifact_item(ui, artifact));
        }
    });
}

/// Stored artifacts are immutable, so one metadata check per path suffices.
fn is_available(ctx: &egui::Context, path: &Path) -> bool {
    let id = egui::Id::new(("artifact-available", path));
    if let Some(available) = ctx.data(|data| data.get_temp::<bool>(id)) {
        return available;
    }
    let available = path.is_file();
    ctx.data_mut(|data| data.insert_temp(id, available));
    available
}

fn artifact_item(ui: &mut Ui, artifact: &PresentedArtifact) {
    let path = Path::new(&artifact.path);
    let available = is_available(ui.ctx(), path);
    ui.horizontal(|ui| {
        let icon = if artifact.is_image() {
            icons::IMAGE
        } else {
            icons::FILE_HTML
        };
        ui.label(medium(icons::with_icon(icon, &artifact.title)).color(palette().TEXT));
        if available && ghost_icon_button(ui, icons::ARROW_SQUARE_OUT, "Open").clicked() {
            request_open(ui.ctx(), path.to_path_buf());
        }
    });
    if let Some(caption) = &artifact.caption {
        ui.label(RichText::new(caption).color(palette().TEXT_MUTED));
    }
    if !available {
        ui.label(
            RichText::new(format!("Artifact unavailable: {}", artifact.artifact_id))
                .size(FONT_SMALL)
                .color(palette().WARNING_FG),
        );
        return;
    }
    if artifact.is_image() {
        if let Some(uri) = crate::panes::file_viewer::image_uri(path) {
            crate::panes::file_viewer::install_image_loader(ui.ctx());
            ui.add_space(SP_1);
            let width = ui.available_width().min(PREVIEW_MAX_WIDTH);
            ui.add(
                egui::Image::new(uri)
                    .max_width(width)
                    .max_height(PREVIEW_MAX_HEIGHT)
                    .maintain_aspect_ratio(true),
            )
            .on_hover_text(&artifact.title);
        }
    } else {
        ui.label(
            RichText::new("HTML mock — open it to view in the browser")
                .size(FONT_SMALL)
                .color(palette().TEXT_MUTED),
        );
    }
}

#[cfg(test)]
mod tests;
