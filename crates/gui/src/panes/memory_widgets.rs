//! Memory words and widgets shared by the embedded workbench's Memory pane and the core
//! client's memory feature (ADR 0045): the filters, the approval badge and button, and the
//! two-step reject.
//!
//! Pure over egui, the theme and protocol types: nothing here reads storage, the runtime or
//! the event bus. Each side says what a lesson is (its id, whether it is approved) and gets
//! back what was pressed, for its owner to carry out.

use egui::{Response, Ui};
use workbench_protocol::{management::MemoryStatus, namespaces::memory::MemoryScope};

use crate::theme::text::badge;

pub const NO_PROJECT: &str = "Select a project to browse memory.";
pub const ALL_STATUSES: &str = "All statuses";
pub const EMPTY: &str = "No matching lessons.";
pub const EVIDENCE: &str = "Evidence";

/// The approval filter: `Some(true)` lists only approved (favorite) lessons, `Some(false)`
/// only unconfirmed ones.
pub const APPROVAL_FILTERS: [(Option<bool>, &str); 3] = [
    (None, "Any approval"),
    (Some(true), "Approved"),
    (Some(false), "Unconfirmed"),
];

const REJECT_TIP: &str = "Stop using this lesson in task prompts. Cannot be undone.";

#[must_use]
pub fn approval_filter_label(approval: Option<bool>) -> &'static str {
    APPROVAL_FILTERS
        .iter()
        .find(|(value, _)| *value == approval)
        .map_or("", |(_, label)| label)
}

#[must_use]
pub const fn status_word(status: MemoryStatus) -> &'static str {
    match status {
        MemoryStatus::Candidate => "candidate",
        MemoryStatus::Validated => "validated",
        MemoryStatus::Promoted => "promoted",
        MemoryStatus::Rejected => "rejected",
    }
}

#[must_use]
pub const fn scope_word(scope: MemoryScope) -> &'static str {
    match scope {
        MemoryScope::Project => "project",
        MemoryScope::Harness => "harness",
        MemoryScope::User => "user",
    }
}

pub fn approval_badge(ui: &mut Ui, favorite: bool) -> Response {
    let (label, tip) = if favorite {
        (
            "Approved",
            "You approved this lesson (favorite). This mark is separate \
             from the status and cannot be rejected until you clear it.",
        )
    } else {
        (
            "Unconfirmed",
            "You have not approved this lesson. Approval is your own \
             mark, separate from the status.",
        )
    };
    ui.label(badge(label)).on_hover_text(tip)
}

#[must_use]
pub const fn approval_action(favorite: bool) -> (&'static str, &'static str) {
    if favorite {
        (
            "Clear approval",
            "Remove your approval mark. The lesson's status is unchanged.",
        )
    } else {
        (
            "Approve",
            "Mark this lesson as approved by you. The status is unchanged, \
             and an approved lesson cannot be rejected.",
        )
    }
}

/// Reject button, or its confirmation once clicked, since rejection cannot be undone.
/// `confirm` holds the lesson waiting for confirmation; `reject` is set to `id` once the
/// rejection is confirmed.
pub fn reject_controls(
    ui: &mut Ui,
    id: &str,
    confirm: &mut Option<String>,
    reject: &mut Option<String>,
) {
    if confirm.as_deref() == Some(id) {
        ui.label("Reject permanently?");
        if ui.button("Confirm reject").clicked() {
            *reject = Some(id.to_owned());
        }
        if ui.button("Cancel").clicked() {
            *confirm = None;
        }
    } else if ui.button("Reject").on_hover_text(REJECT_TIP).clicked() {
        *confirm = Some(id.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::{Harness, kittest::Queryable};

    #[test]
    fn the_approval_badge_names_the_mark_and_explains_it_on_hover() {
        let mut harness = Harness::new_ui(|ui| {
            approval_badge(ui, true);
            approval_badge(ui, false);
        });
        harness.run();
        harness.get_by_label("Approved").hover();
        harness.run();
        harness.get_by_label_contains("cannot be rejected until you clear it");
        harness.get_by_label("Unconfirmed");
        assert_eq!(approval_action(true).0, "Clear approval");
        assert_eq!(approval_action(false).0, "Approve");
        assert_eq!(approval_filter_label(Some(false)), "Unconfirmed");
    }

    #[derive(Default)]
    struct Confirm {
        confirm: Option<String>,
        reject: Option<String>,
    }

    #[test]
    fn a_reject_asks_for_confirmation_and_cancel_takes_it_back() {
        let mut harness = Harness::new_ui_state(
            |ui, state: &mut Confirm| {
                ui.horizontal(|ui| {
                    reject_controls(ui, "lesson-1", &mut state.confirm, &mut state.reject);
                });
            },
            Confirm::default(),
        );
        harness.run();
        harness.get_by_label("Reject").click();
        harness.run();
        assert_eq!(harness.state().confirm.as_deref(), Some("lesson-1"));
        assert!(
            harness.state().reject.is_none(),
            "one click rejects nothing"
        );
        harness.get_by_label("Reject permanently?");
        harness.get_by_label("Cancel").click();
        harness.run();
        assert!(harness.state().confirm.is_none());
        harness.get_by_label("Reject").click();
        harness.run();
        harness.get_by_label("Confirm reject").click();
        harness.run();
        assert_eq!(harness.state().reject.as_deref(), Some("lesson-1"));
    }
}
