//! One agent run as a card of the Subagents column, and a run's working note.
//!
//! Pure drawing shared by the embedded workbench and the core client: the run
//! arrives as borrowed text ([`RunCardHeader`], [`NoteRevision`]) whatever it
//! was read from, and a press comes back as a `bool` for the owner to act on.
use crate::theme::icons;
use crate::theme::text::{badge, muted, semibold};
use crate::theme::tokens::palette;
use crate::theme::widgets::{copy_id_button, ghost, halo_dot, soft_frame, surface_frame};

/// The first line of a run card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunCardHeader<'a> {
    pub run_id: &'a str,
    pub name: &'a str,
    /// `Some` draws the name as a link with this hover; `None` as plain text.
    pub drill_hint: Option<&'a str>,
    pub color: egui::Color32,
    /// The dot carries a halo while the run is working.
    pub active: bool,
    /// `{role} · {status}`.
    pub status: &'a str,
    pub status_hover: Option<String>,
    /// `{model} · {provider}`.
    pub model: &'a str,
    pub model_hover: Option<String>,
}

/// A card of the column: its own id scope, a soft surface, the full width.
pub fn run_card<R>(ui: &mut egui::Ui, run_id: &str, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.push_id(run_id, |ui| {
        soft_frame(palette().SURFACE)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                add(ui)
            })
            .inner
    })
    .inner
}

/// The dot, the name, the copy button, the role and status, and the model.
/// Returns whether the name was clicked; a plain name never is.
pub fn run_card_header(ui: &mut egui::Ui, header: &RunCardHeader<'_>) -> bool {
    let mut clicked = false;
    ui.horizontal_wrapped(|ui| {
        halo_dot(ui, header.color, header.active);
        match header.drill_hint {
            Some(hint) => {
                clicked = ui
                    .add(egui::Link::new(semibold(header.name).color(palette().TEXT)))
                    .on_hover_text(hint)
                    .clicked();
            }
            None => {
                ui.label(semibold(header.name).color(palette().TEXT));
            }
        }
        copy_id_button(ui, "Copy run ID", header.run_id);
        let status = ui.label(muted(header.status));
        if let Some(hover) = &header.status_hover {
            status.on_hover_text(hover);
        }
        let model = ui.label(muted(header.model));
        if let Some(hover) = &header.model_hover {
            model.on_hover_text(hover);
        }
    });
    clicked
}

pub fn open_pane_button(ui: &mut egui::Ui) -> bool {
    let open = ui.add(ghost(muted(icons::with_icon(
        icons::ARROW_SQUARE_OUT,
        "Open pane",
    ))));
    open.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Open pane"));
    open.clicked()
}

/// Stop with a two-click confirmation per run, using egui's frame time. A run
/// that cannot be stopped draws nothing and forgets a pending confirmation.
pub fn stop_run_button(ui: &mut egui::Ui, run_id: &str, stoppable: bool) -> bool {
    let id = ui.make_persistent_id(("stop-run-confirmation", run_id));
    if !stoppable {
        ui.ctx().data_mut(|data| data.remove::<f64>(id));
        return false;
    }
    let now = ui.input(|input| input.time);
    let deadline = ui.ctx().data_mut(|data| {
        let deadline = data.get_temp::<f64>(id);
        if deadline.is_some_and(|deadline| now >= deadline) {
            data.remove::<f64>(id);
            None
        } else {
            deadline
        }
    });
    let text = if deadline.is_some() {
        egui::RichText::new("Confirm?").color(palette().WARNING_FG)
    } else {
        muted("Stop")
    };
    let response = ui.add(ghost(text)).on_hover_text("Stop this agent run");
    if response.clicked() {
        if deadline.is_some() {
            ui.ctx().data_mut(|data| data.remove::<f64>(id));
            return true;
        }
        ui.ctx().data_mut(|data| data.insert_temp(id, now + 2.0));
        ui.ctx().request_repaint();
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_secs(2));
    } else if let Some(deadline) = deadline {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_secs_f64(deadline - now));
    }
    false
}

/// A worker's role names its category: `Worker(quick)`.
pub fn role_label(role: &str, category: Option<&str>) -> String {
    match category {
        Some(category) if role.eq_ignore_ascii_case("worker") => format!("Worker({category})"),
        _ => role.to_owned(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoteRevision<'a> {
    pub seq: u64,
    pub body: &'a str,
}

/// A run's working note, folded until it is opened. `revisions` are oldest
/// first; the last is the current one. `truncated` says the current body is
/// only the start of the note.
pub fn working_note(ui: &mut egui::Ui, key: &str, revisions: &[NoteRevision<'_>], truncated: bool) {
    let Some((current, previous)) = revisions.split_last() else {
        return;
    };
    let id = ui.id().with(("ledger-expanded", key));
    let mut expanded = ui.data(|data| data.get_temp::<bool>(id).unwrap_or(false));
    surface_frame(palette().SURFACE).show(ui, |ui| {
        let arrow = if expanded { "v" } else { ">" };
        if ui
            .add(
                egui::Button::new(badge(format!("{arrow} Working note")))
                    .frame(false)
                    .wrap(),
            )
            .clicked()
        {
            expanded = !expanded;
            ui.data_mut(|data| data.insert_temp(id, expanded));
        }
        if !expanded {
            return;
        }
        ui.label(if current.body.is_empty() {
            "Current note cleared".into()
        } else {
            format!("Current revision {}: {}", current.seq, current.body)
        });
        if truncated {
            ui.label(muted("Only the start of this note is shown."));
        }
        if !previous.is_empty() {
            ui.collapsing("Previous revisions", |ui| {
                for revision in previous.iter().rev() {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(format!(
                                "Revision {}: {}",
                                revision.seq,
                                if revision.body.is_empty() {
                                    "(cleared)"
                                } else {
                                    revision.body
                                }
                            ))
                            .color(palette().TEXT),
                        )
                        .wrap(),
                    );
                }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::{Harness, kittest::Queryable};

    #[test]
    fn a_worker_names_its_category_and_other_roles_stay_as_they_are() {
        assert_eq!(role_label("worker", Some("quick")), "Worker(quick)");
        assert_eq!(role_label("Worker", Some("deep")), "Worker(deep)");
        assert_eq!(role_label("worker", None), "worker");
        assert_eq!(role_label("reviewer", Some("quick")), "reviewer");
    }

    fn note(revisions: Vec<(u64, &'static str)>, truncated: bool) -> Harness<'static> {
        let mut harness = Harness::new_ui(move |ui| {
            let revisions: Vec<_> = revisions
                .iter()
                .map(|(seq, body)| NoteRevision { seq: *seq, body })
                .collect();
            working_note(ui, "run-a", &revisions, truncated);
        });
        harness.run_steps(2);
        harness
    }

    #[test]
    fn a_working_note_folds_and_says_when_it_is_cut_or_cleared() {
        // Given: a truncated current revision, folded.
        let mut harness = note(vec![(4, "the start")], true);
        assert!(
            harness
                .query_by_label("Current revision 4: the start")
                .is_none()
        );
        // When: it is opened.
        harness.get_by_label("> Working note").click();
        harness.run_steps(2);
        // Then: the revision and the truncation line show, and nothing older.
        harness.get_by_label("Current revision 4: the start");
        harness.get_by_label("Only the start of this note is shown.");
        assert!(harness.query_by_label("Previous revisions").is_none());

        // Given/When: a whole note whose current revision cleared it, opened.
        // Then: it says so, without a truncation line, with the older revision kept.
        let mut harness = note(vec![(1, "first"), (2, "")], false);
        harness.get_by_label("> Working note").click();
        harness.run_steps(2);
        harness.get_by_label("Current note cleared");
        assert!(
            harness
                .query_by_label("Only the start of this note is shown.")
                .is_none()
        );
        harness.get_by_label("Previous revisions");
    }

    #[test]
    fn no_revision_draws_no_note() {
        let harness = note(vec![], false);
        assert!(harness.query_by_label_contains("Working note").is_none());
    }
}
