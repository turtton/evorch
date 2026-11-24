//! Team board words and the task card shared by the embedded workbench's Team pane and the
//! core client's teams feature (ADR 0045).
//!
//! Pure over egui, the theme and protocol types: nothing here reads the runtime's board.
//! Each side builds a [`TaskCardView`] from what it read.

use egui::{Label, RichText, Ui};
use workbench_protocol::namespaces::teams::TeamTaskState;

use crate::theme::text::muted;
use crate::theme::tokens::{SP_1, palette};
use crate::theme::widgets::surface_frame;

pub const HEADING: &str = "Team";
pub const EMPTY: &str = "Team mode is disabled or no team has started.";
pub const COORDINATOR: &str = "Coordinator";
pub const PATHS: &str = "Paths";

#[must_use]
pub const fn state_word(state: TeamTaskState) -> &'static str {
    match state {
        TeamTaskState::Ready => "Ready",
        TeamTaskState::Claimed => "Claimed",
        TeamTaskState::Complete => "Complete",
    }
}

#[must_use]
pub fn owner_tip(generation: u64, lease_deadline_ms: Option<u64>) -> String {
    let mut tip = format!("Assigned execution · Generation {generation}");
    if let Some(deadline) = lease_deadline_ms {
        tip.push_str(&format!(" · lease deadline {deadline} ms"));
    }
    tip
}

pub struct TaskCardView<'a> {
    pub title: &'a str,
    pub state: TeamTaskState,
    pub owner: Option<&'a str>,
    pub owner_tip: Option<String>,
    pub paths: &'a [String],
}

/// One task as a card: its title, state and owner on one row, then `extra`, then its paths.
pub fn task_card(ui: &mut Ui, view: &TaskCardView<'_>, extra: impl FnOnce(&mut Ui)) {
    surface_frame(palette().SURFACE).show(ui, |ui| {
        ui.set_min_width(ui.available_width());
        ui.horizontal_wrapped(|ui| {
            ui.add(Label::new(RichText::new(view.title).strong().color(palette().TEXT)).wrap());
            // Wrapped like the rest of the row, so a long owner stays inside a narrow card.
            ui.label(state_word(view.state));
            if let Some(owner) = view.owner {
                let response = ui.label(owner);
                if let Some(tip) = &view.owner_tip {
                    response.on_hover_text(tip);
                }
            }
        });
        extra(ui);
        if !view.paths.is_empty() {
            ui.label(muted(PATHS).strong());
            for path in view.paths {
                ui.add(Label::new(path).wrap());
            }
        }
    });
    ui.add_space(SP_1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::{Harness, kittest::Queryable};

    #[test]
    fn a_card_keeps_its_title_state_and_owner_on_one_row() {
        let paths = vec!["src/lib.rs".to_owned()];
        let mut harness = Harness::builder()
            .with_size(egui::vec2(600.0, 300.0))
            .build_ui(move |ui| {
                task_card(
                    ui,
                    &TaskCardView {
                        title: "task-1",
                        state: TeamTaskState::Claimed,
                        owner: Some("worker-2"),
                        owner_tip: Some(owner_tip(3, None)),
                        paths: &paths,
                    },
                    |ui| {
                        ui.label("extra line");
                    },
                );
            });
        harness.run();
        let title = harness.get_by_label("task-1").rect();
        let state = harness.get_by_label("Claimed").rect();
        let owner = harness.get_by_label("worker-2").rect();
        assert!((title.center().y - state.center().y).abs() < 1.0);
        assert!((title.center().y - owner.center().y).abs() < 1.0);
        let extra = harness.get_by_label("extra line").rect();
        let path = harness.get_by_label("src/lib.rs").rect();
        assert!(title.max.y <= extra.min.y && extra.max.y <= path.min.y);
        harness.get_by_label(PATHS);
        harness.get_by_label("worker-2").hover();
        harness.run();
        harness.get_by_label("Assigned execution · Generation 3");
        assert_eq!(
            owner_tip(1, Some(5_000)),
            "Assigned execution · Generation 1 · lease deadline 5000 ms"
        );
    }

    #[test]
    fn a_long_owner_wraps_inside_a_narrow_card() {
        const OWNER: &str = "run-01JC0ABCDEFGHJKMNPQRSTVWXY";
        let mut harness = Harness::builder()
            .with_size(egui::vec2(160.0, 300.0))
            .build_ui(|ui| {
                task_card(
                    ui,
                    &TaskCardView {
                        title: "task-1",
                        state: TeamTaskState::Claimed,
                        owner: Some(OWNER),
                        owner_tip: None,
                        paths: &[],
                    },
                    |_| {},
                );
            });
        harness.run();
        assert!(harness.get_by_label(OWNER).rect().max.x <= 160.0);
    }
}
