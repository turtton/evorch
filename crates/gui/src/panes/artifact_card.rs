//! Conversation cards for artifacts presented by the conversation owner.
//!
//! Opening goes through a request queue instead of `OpenUrl`: local `file://`
//! links are consumed by the in-app file viewer, which would show HTML source.

use std::path::{Path, PathBuf};
use std::time::Duration;

use egui::{RichText, Ui};
use event_bus::{ArtifactPresentation, PresentedArtifact};

use crate::model::html_snapshots::{HtmlSnapshots, SnapshotState};
use crate::theme::icons;
use crate::theme::text::medium;
use crate::theme::tokens::{FONT_SMALL, SP_1, SP_2, palette};
use crate::theme::widgets::{ghost_icon_button, soft_frame};

const PREVIEW_MAX_WIDTH: f32 = 640.0;
const PREVIEW_MAX_HEIGHT: f32 = 480.0;
const AVAILABILITY_RECHECK_INTERVAL: Duration = Duration::from_secs(1);

fn open_requests_id() -> egui::Id {
    egui::Id::new("artifact-open-requests")
}

fn external_open_requests_id() -> egui::Id {
    egui::Id::new("artifact-external-open-requests")
}

/// Open outside evorch, even when the host has an in-app HTML preview.
fn request_external_open(ctx: &egui::Context, path: PathBuf) {
    ctx.data_mut(|data| {
        data.get_temp_mut_or_default::<Vec<PathBuf>>(external_open_requests_id())
            .push(path);
    });
}

pub fn take_external_open_requests(ctx: &egui::Context) -> Vec<PathBuf> {
    ctx.data_mut(|data| {
        data.remove_temp::<Vec<PathBuf>>(external_open_requests_id())
            .unwrap_or_default()
    })
}

/// Ask the host to open an artifact, allowing the in-app HTML preview.
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

#[derive(Clone, Copy)]
struct Availability {
    available: bool,
    retry_at: f64,
}

fn is_available(ctx: &egui::Context, path: &Path) -> bool {
    is_available_at(ctx, path, ctx.input(|input| input.time), Path::is_file)
}

/// Stored files are immutable, but an unavailable store can recover. Limit
/// metadata I/O during repeated frames while scheduling a repaint for recovery.
fn is_available_at(
    ctx: &egui::Context,
    path: &Path,
    now: f64,
    check_file: impl FnOnce(&Path) -> bool,
) -> bool {
    let id = egui::Id::new(("artifact-available", path));
    let cached = ctx.data(|data| data.get_temp::<Availability>(id));
    let availability = match cached {
        Some(cached) if cached.available || now < cached.retry_at => cached,
        _ => {
            let checked = Availability {
                available: check_file(path),
                retry_at: now + AVAILABILITY_RECHECK_INTERVAL.as_secs_f64(),
            };
            ctx.data_mut(|data| data.insert_temp(id, checked));
            checked
        }
    };
    if !availability.available {
        ctx.request_repaint_after(Duration::from_secs_f64(
            (availability.retry_at - now).max(0.0),
        ));
    }
    availability.available
}

fn artifact_item(ui: &mut Ui, artifact: &PresentedArtifact) {
    let path = Path::new(&artifact.path);
    let available = is_available(ui.ctx(), path);
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let icon = if artifact.is_image() {
                icons::IMAGE
            } else {
                icons::FILE_HTML
            };
            if ui
                .add_enabled_ui(available, |ui| {
                    ghost_icon_button(ui, icons::ARROW_SQUARE_OUT, "Open")
                        .on_hover_text("Open in the default browser or image viewer")
                })
                .inner
                .clicked()
            {
                request_external_open(ui.ctx(), path.to_path_buf());
            }
            ui.add(
                egui::Label::new(
                    medium(icons::with_icon(icon, &artifact.title)).color(palette().TEXT),
                )
                .truncate(),
            )
            .on_hover_text(&artifact.title);
        });
    });
    if let Some(caption) = &artifact.caption {
        ui.label(RichText::new(caption).color(palette().TEXT_MUTED));
    }
    if !available {
        ui.label(
            RichText::new(format!(
                "Artifact unavailable: {} (file missing or inaccessible)",
                artifact.artifact_id
            ))
            .size(FONT_SMALL)
            .color(palette().WARNING_FG),
        );
        return;
    }
    if artifact.is_image() {
        if let Some(preview) = image_preview(ui, path, &artifact.title) {
            let preview = preview
                .on_hover_cursor(egui::CursorIcon::ZoomIn)
                .on_hover_text(format!(
                    "Enlarge {} (right-click to open it in another app)",
                    artifact.title
                ));
            if preview.clicked() {
                crate::panes::image_viewer::open(
                    ui.ctx(),
                    path.to_path_buf(),
                    artifact.title.clone(),
                );
            }
            crate::panes::image_viewer::context_menu(&preview, path);
        }
    } else if artifact.media_type == "text/html" {
        if let Some(snapshots) = HtmlSnapshots::installed(ui.ctx()) {
            html_snapshot(ui, &snapshots, artifact, path);
        } else {
            preview_note(
                ui,
                "Preview unavailable: HTML renderer (CEF) is not installed",
            );
        }
    } else {
        preview_note(
            ui,
            format!(
                "Preview unavailable: unsupported media type ({})",
                artifact.media_type
            ),
        );
    }
}

fn preview_note(ui: &mut Ui, text: impl Into<String>) {
    ui.label(
        RichText::new(text.into())
            .size(FONT_SMALL)
            .color(palette().TEXT_MUTED),
    );
}

/// egui normally puts load errors in a hover tooltip on a tiny image icon.
/// Keep the actual bounded loader's reason visible in the conversation instead.
fn image_preview(ui: &mut Ui, path: &Path, title: &str) -> Option<egui::Response> {
    let Some(uri) = crate::panes::file_viewer::image_uri(path) else {
        preview_note(ui, "Preview unavailable: invalid image path");
        return None;
    };
    crate::panes::file_viewer::install_image_loader(ui.ctx());
    let image = egui::Image::new(uri)
        .alt_text(format!("{title} preview"))
        // Remaining height can be zero below a transcript's scroll viewport.
        .fit_to_original_size(1.0)
        .max_width(ui.available_width().min(PREVIEW_MAX_WIDTH))
        .max_height(PREVIEW_MAX_HEIGHT)
        .maintain_aspect_ratio(true)
        .sense(egui::Sense::click());
    match image.load_for_size(ui.ctx(), ui.available_size()) {
        Ok(egui::load::TexturePoll::Ready { .. }) => {
            ui.add_space(SP_1);
            Some(ui.add(image))
        }
        Ok(egui::load::TexturePoll::Pending { .. }) => {
            preview_note(ui, "Loading preview…");
            None
        }
        Err(error) => {
            preview_note(ui, format!("Preview unavailable: {error}"));
            None
        }
    }
}

fn html_snapshot(
    ui: &mut Ui,
    snapshots: &HtmlSnapshots,
    artifact: &PresentedArtifact,
    path: &Path,
) {
    match snapshots.state(&artifact.sha256, path) {
        SnapshotState::Ready(png) => {
            let Some(preview) = image_preview(ui, &png, &artifact.title) else {
                return;
            };
            let preview = preview
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text(format!("Open {}", artifact.title));
            if preview.clicked() {
                request_open(ui.ctx(), path.to_path_buf());
            }
        }
        SnapshotState::Pending => {
            preview_note(ui, "Rendering preview…");
            // The host drains snapshot requests after the card has rendered.
            let id = ui.id().with(("snapshot-requested", &artifact.sha256));
            let already_requested = ui.ctx().data_mut(|data| {
                let previous = data.get_temp::<bool>(id).unwrap_or(false);
                data.insert_temp(id, true);
                previous
            });
            if !already_requested {
                ui.ctx().request_repaint();
            }
        }
        SnapshotState::Failed(error) => preview_note(ui, format!("Preview unavailable: {error}")),
    }
}

#[cfg(test)]
mod tests;
