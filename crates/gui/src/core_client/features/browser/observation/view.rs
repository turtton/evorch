//! What the observation shows. Observation only: the run list, the tab list and
//! the activity are labels; nothing here changes the page or the agent's tab.
use workbench_protocol::namespaces::browser::observation::{
    ObservationActivity, ObservationScope, ObservationSnapshot, ObservationTarget,
};

use super::{ObservationFeature, Runs, Shown, pixels::one_line};

/// The newest activity lines laid over the picture, as the previous pane did.
const OVERLAY_LINES: usize = 6;

pub(super) enum Click {
    View(ObservationTarget),
    Reload,
}

/// What is on screen for the run being watched: the live watch, or the still
/// picture it left.
struct Current<'a> {
    snapshot: Option<&'a ObservationSnapshot>,
    shown: Option<&'a Shown>,
    live: bool,
    note: Option<&'a str>,
}

impl ObservationFeature {
    fn current(&self) -> Option<Current<'_>> {
        let run = &self.target.as_ref()?.run;
        let stopped = self.stopped.iter().find(|s| s.scope.run == *run);
        if let Some(live) = self.live.as_ref().filter(|l| l.scope.run == *run) {
            return Some(match (&live.shown, stopped) {
                (None, Some(stopped)) => Current {
                    snapshot: stopped.snapshot.as_ref(),
                    shown: Some(&stopped.shown),
                    live: true,
                    note: None,
                },
                _ => Current {
                    snapshot: live.snapshot.as_ref(),
                    shown: live.shown.as_ref(),
                    live: true,
                    note: None,
                },
            });
        }
        Some(Current {
            snapshot: stopped.and_then(|s| s.snapshot.as_ref()),
            shown: stopped.map(|s| &s.shown),
            live: false,
            note: stopped.and_then(|s| s.note.as_deref()),
        })
    }

    pub(super) fn apply(&mut self, click: Click) {
        match click {
            Click::View(target) => self.choose(target),
            Click::Reload => {
                self.hold = None;
                self.relist = true;
            }
        }
    }

    pub(super) fn draw(&self, ui: &mut egui::Ui) -> Option<Click> {
        let mut click = None;
        ui.strong("Agent browser");
        let selected = self.target.as_ref().map(|t| &t.run);
        match &self.runs {
            Runs::Unknown | Runs::Loading => {
                ui.weak("Looking for agent pages...");
            }
            Runs::Failed(message) => {
                ui.colored_label(ui.visuals().error_fg_color, one_line(message));
            }
            Runs::Loaded(scopes) => {
                if scopes.is_empty() && self.stopped.is_empty() {
                    ui.weak(
                        "No agent page is open in this conversation. A page appears here when an agent uses the browser tools.",
                    );
                }
                ui.horizontal_wrapped(|ui| {
                    for scope in scopes {
                        self.run_label(ui, scope, selected, false, &mut click);
                    }
                    for stopped in &self.stopped {
                        if !scopes.iter().any(|s| s.run == stopped.scope.run) {
                            self.run_label(ui, &stopped.scope, selected, true, &mut click);
                        }
                    }
                });
            }
        }
        if ui.small_button("Refresh agent pages").clicked() {
            click = Some(Click::Reload);
        }
        let current = self.current();
        if let Some(hold) = &self.hold
            && current
                .as_ref()
                .is_none_or(|c| c.note != Some(hold.message.as_str()))
        {
            ui.colored_label(ui.visuals().error_fg_color, one_line(&hold.message));
        }
        if let Some(current) = current {
            self.draw_current(ui, &current);
        }
        click
    }

    fn run_label(
        &self,
        ui: &mut egui::Ui,
        scope: &ObservationScope,
        selected: Option<&workbench_protocol::ids::RunId>,
        stopped: bool,
        click: &mut Option<Click>,
    ) {
        let name = one_line(scope.run.as_str());
        let text = if stopped {
            format!("{name} (stopped)")
        } else {
            name
        };
        let on = selected == Some(&scope.run);
        if ui.selectable_label(on, text).clicked() && !on {
            *click = Some(Click::View(ObservationTarget {
                project: scope.project.clone(),
                thread: scope.thread.clone(),
                run: scope.run.clone(),
            }));
        }
    }

    fn draw_current(&self, ui: &mut egui::Ui, current: &Current<'_>) {
        let snapshot = current.snapshot;
        let shown_tab = current.shown.map(|s| &s.tab);
        let active = shown_tab.or_else(|| snapshot.and_then(|s| s.active_tab.as_ref()));
        if let Some(snapshot) = snapshot {
            if let Some(tab) = snapshot.tabs.iter().find(|t| Some(&t.id) == active) {
                ui.weak(format!(
                    "Showing the active tab {} ({})",
                    one_line(&tab.title),
                    one_line(&tab.url)
                ));
            }
            if snapshot.tabs.len() > 1 {
                for tab in &snapshot.tabs {
                    let mark = if Some(&tab.id) == active { "> " } else { "  " };
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(format!(
                                "{mark}{} {}",
                                one_line(&tab.title),
                                one_line(&tab.url)
                            ))
                            .weak(),
                        )
                        .truncate(),
                    );
                }
            }
        }
        if let Some(note) = current.note {
            ui.weak(one_line(note));
        }
        let activity = snapshot.map_or(&[][..], |s| s.activity.as_slice());
        let recent = &activity[activity.len().saturating_sub(OVERLAY_LINES)..];
        match current.shown.and_then(|s| s.picture.texture()) {
            Some(texture) => {
                let image = ui.add(
                    egui::Image::new(texture)
                        .max_size(ui.available_size())
                        .maintain_aspect_ratio(true),
                );
                if !recent.is_empty() {
                    overlay(ui, image.rect, recent);
                }
            }
            None => {
                if current.live {
                    ui.weak("Waiting for the first frame of this page...");
                }
                activity_lines(ui, recent);
            }
        }
    }
}

fn overlay(ui: &mut egui::Ui, image: egui::Rect, recent: &[ObservationActivity]) {
    let height = (recent.len() as f32 * 18.0 + 12.0).min(image.height());
    let area = egui::Rect::from_min_max(
        egui::pos2(image.left(), image.bottom() - height),
        image.right_bottom(),
    );
    ui.painter()
        .rect_filled(area, 0.0, egui::Color32::from_black_alpha(170));
    ui.scope_builder(egui::UiBuilder::new().max_rect(area.shrink(6.0)), |ui| {
        activity_lines(ui, recent);
    });
}

pub(super) fn activity_text(entry: &ObservationActivity) -> (String, bool) {
    match entry {
        ObservationActivity::Action { label, error: None } => (one_line(label), false),
        ObservationActivity::Action {
            label,
            error: Some(error),
        } => (
            format!("{} failed: {}", one_line(label), one_line(error)),
            true,
        ),
        ObservationActivity::Blocked(request) => (format!("blocked {}", one_line(request)), true),
        ObservationActivity::Dialog(dialog) => (format!("dialog {}", one_line(dialog)), false),
    }
}

fn activity_lines(ui: &mut egui::Ui, recent: &[ObservationActivity]) {
    for entry in recent {
        let (text, failed) = activity_text(entry);
        let color = if failed {
            ui.visuals().error_fg_color
        } else {
            egui::Color32::WHITE
        };
        ui.add(egui::Label::new(egui::RichText::new(text).color(color).monospace()).truncate());
    }
}
