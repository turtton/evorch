//! The "Link PR…" dialog (ADR 0043): a URL or `owner/repo#N`, with a warning when the pull
//! request's repository is not the project's. A warning never blocks the link.

use crate::app::pull_requests::LinkDialog;
pub use crate::panes::pull_request_widgets::{
    DestinationDialogAction, INPUT_LABEL, LinkDialogAction, MOVE_LABEL, Reading,
};
use crate::panes::pull_request_widgets::{destination_dialog_view, link_dialog_view};

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
    let reading = read_locator(&dialog.input, dialog.project_slug.as_deref());
    link_dialog_view(ctx, &mut dialog.input, &reading, thread_loaded, None)
}

/// The confirmation before auto fix moves to this thread from the one that has it.
pub fn destination_dialog(
    ctx: &egui::Context,
    dialog: &crate::app::pull_requests::DestinationDialog,
) -> Option<DestinationDialogAction> {
    destination_dialog_view(ctx, &dialog.previous_label)
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
