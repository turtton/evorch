//! The image a successful `read` call returned, shown in its card.
//!
//! The card draws it from the original file, through the bounded loader the
//! file viewer and artifact cards already use, so decoding stays off the UI
//! thread and its completion reaches the transcript's async-layout versions.
//! A card is only drawn while its row is on screen and its group is open, so
//! nothing is loaded before then. The result's bytes never reach the card.

use std::path::{Path, PathBuf};

use egui::{RichText, Ui};

use crate::model::transcript::ToolStatus;
use crate::theme::tokens::{FONT_SMALL, palette};

/// Shows the thumbnail of a finished `read` call that returned an image.
/// Anything else, including a call that failed, draws nothing.
pub(super) fn show(
    ui: &mut Ui,
    tool_name: &str,
    status: &ToolStatus,
    is_error: bool,
    detail: Option<&serde_json::Value>,
    repo_root: Option<&Path>,
) {
    if tool_name != "read" || is_error || !matches!(status, ToolStatus::Succeeded) {
        return;
    }
    let Some(detail) = detail else {
        return;
    };
    let is_image = detail
        .get("media_type")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|media_type| media_type.starts_with("image/"));
    let Some(path) = detail
        .get("path")
        .and_then(serde_json::Value::as_str)
        .filter(|_| is_image)
    else {
        return;
    };
    let Some(path) = absolute(path, repo_root) else {
        note(
            ui,
            "Image not shown: its path is relative to an unknown folder",
        );
        return;
    };
    let name = path.file_name().map_or_else(
        || "Image".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    // The thumbnail is not a control, so it does not sense clicks.
    crate::panes::artifact_card::image_preview(ui, &path, &name, egui::Sense::hover());
}

/// The call's path as the tool resolved it: relative paths are the project's.
fn absolute(path: &str, repo_root: Option<&Path>) -> Option<PathBuf> {
    let path = Path::new(path);
    if path.is_absolute() {
        Some(path.to_path_buf())
    } else {
        repo_root.map(|root| root.join(path))
    }
}

fn note(ui: &mut Ui, text: &str) {
    ui.label(
        RichText::new(text)
            .size(FONT_SMALL)
            .color(palette().TEXT_MUTED),
    );
}
