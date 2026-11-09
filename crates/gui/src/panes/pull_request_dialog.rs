//! The "Link PR…" dialog (ADR 0043): a URL or `owner/repo#N`, with a warning when the pull
//! request's repository is not the project's. A warning never blocks the link.

use egui::{Align, Layout};

use crate::app::pull_requests::LinkDialog;
use crate::theme::text::{h3, muted};
use crate::theme::tokens::*;
use crate::theme::widgets::{ghost, primary_button, surface_frame};

pub const INPUT_LABEL: &str = "Pull request (URL or owner/repo#N)";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkDialogAction {
    Link(String),
    Close,
}

/// What the dialog says about the typed locator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reading {
    /// Nothing typed yet.
    Empty,
    /// Not a github.com pull request; the text is the reason.
    Invalid(String),
    /// A pull request, with a warning when its repository differs from the project's.
    Valid {
        label: String,
        warning: Option<String>,
    },
}

/// Reads the typed locator against the project's repository (`owner/name`, when known).
#[must_use]
pub fn read_locator(input: &str, project_slug: Option<&str>) -> Reading {
    let input = input.trim();
    if input.is_empty() {
        return Reading::Empty;
    }
    match runtime::pull_requests::parse_locator(input) {
        Err(error) => Reading::Invalid(error.to_string()),
        Ok(key) => {
            let warning = project_slug
                .filter(|slug| slug.contains('/'))
                .filter(|slug| !slug.eq_ignore_ascii_case(&key.repository))
                .map(|slug| {
                    format!(
                        "This pull request is in {}, but the project's origin is {slug}.",
                        key.repository
                    )
                });
            Reading::Valid {
                label: crate::model::pull_requests::key_label(&key),
                warning,
            }
        }
    }
}

pub fn link_dialog(
    ctx: &egui::Context,
    dialog: &mut LinkDialog,
    thread_loaded: bool,
) -> Option<LinkDialogAction> {
    let mut action = None;
    let reading = read_locator(&dialog.input, dialog.project_slug.as_deref());
    let modal = egui::Modal::new(egui::Id::new("pull-request-link-dialog"))
        .backdrop_color(palette().OVERLAY)
        .frame(surface_frame(palette().SURFACE_RAISED))
        .show(ctx, |ui| {
            ui.set_width(
                (ctx.viewport_rect().width() * 0.5).min(PROVIDER_MODAL_MAX_WIDTH) - SP_4 * 4.0,
            );
            ui.spacing_mut().item_spacing = egui::vec2(SP_2, SP_2);
            ui.label(h3("Link pull request"));
            ui.label(muted(INPUT_LABEL));
            let edit = ui.add(
                egui::TextEdit::singleline(&mut dialog.input)
                    .hint_text("https://github.com/owner/repo/pull/123")
                    .desired_width(f32::INFINITY),
            );
            edit.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, INPUT_LABEL)
            });
            if !edit.has_focus() && dialog.input.is_empty() {
                edit.request_focus();
            }
            match &reading {
                Reading::Empty => {}
                Reading::Invalid(reason) => {
                    ui.colored_label(palette().ERROR_FG, reason);
                }
                Reading::Valid { label, warning } => {
                    ui.label(label);
                    if let Some(warning) = warning {
                        ui.colored_label(palette().WARNING_FG, warning);
                    }
                }
            }
            let ready = thread_loaded && matches!(reading, Reading::Valid { .. });
            let submitted =
                edit.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let link = ui
                    .add_enabled_ui(ready, |ui| primary_button(ui, "Link"))
                    .inner;
                if (link.clicked() || submitted) && ready {
                    action = Some(LinkDialogAction::Link(dialog.input.trim().to_owned()));
                }
                if ui.add(ghost("Cancel")).clicked() {
                    action = Some(LinkDialogAction::Close);
                }
            });
        });
    if action.is_none() && modal.should_close() {
        action = Some(LinkDialogAction::Close);
    }
    action
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DestinationDialogAction {
    Move,
    Cancel,
}

pub const MOVE_LABEL: &str = "Move here";

/// The confirmation before auto fix moves to this thread from the one that has it.
pub fn destination_dialog(
    ctx: &egui::Context,
    dialog: &crate::app::pull_requests::DestinationDialog,
) -> Option<DestinationDialogAction> {
    let mut action = None;
    let modal = egui::Modal::new(egui::Id::new("pull-request-destination-dialog"))
        .backdrop_color(palette().OVERLAY)
        .frame(surface_frame(palette().SURFACE_RAISED))
        .show(ctx, |ui| {
            ui.set_width(
                (ctx.viewport_rect().width() * 0.5).min(PROVIDER_MODAL_MAX_WIDTH) - SP_4 * 4.0,
            );
            ui.spacing_mut().item_spacing = egui::vec2(SP_2, SP_2);
            ui.label(h3("Move auto fix here?"));
            ui.label(format!(
                "This pull request's CI failures go to \u{201c}{}\u{201d}. Turning auto fix on \
                 here sends them to this thread instead.",
                dialog.previous_label
            ));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if primary_button(ui, MOVE_LABEL).clicked() {
                    action = Some(DestinationDialogAction::Move);
                }
                if ui.add(ghost("Cancel")).clicked() {
                    action = Some(DestinationDialogAction::Cancel);
                }
            });
        });
    if action.is_none() && modal.should_close() {
        action = Some(DestinationDialogAction::Cancel);
    }
    action
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repository_mismatch_warns_but_still_reads_as_valid() {
        let reading = read_locator("other/repo#7", Some("turtton/evorch"));
        let Reading::Valid { label, warning } = reading else {
            panic!("a pull request");
        };
        assert_eq!(label, "other/repo#7");
        assert!(warning.unwrap().contains("turtton/evorch"));
        let same = read_locator(
            "https://github.com/turtton/evorch/pull/7",
            Some("Turtton/Evorch"),
        );
        assert!(matches!(same, Reading::Valid { warning: None, .. }));
    }

    #[test]
    fn an_unknown_project_repository_warns_about_nothing() {
        assert!(matches!(
            read_locator("a/b#1", Some("evorch")),
            Reading::Valid { warning: None, .. }
        ));
        assert!(matches!(
            read_locator("a/b#1", None),
            Reading::Valid { warning: None, .. }
        ));
    }

    #[test]
    fn text_that_is_not_a_pull_request_is_invalid() {
        assert_eq!(read_locator("  ", None), Reading::Empty);
        assert!(matches!(
            read_locator("not a pr", None),
            Reading::Invalid(_)
        ));
        assert!(matches!(
            read_locator("https://gitlab.com/a/b/pull/1", None),
            Reading::Invalid(_)
        ));
    }
}
