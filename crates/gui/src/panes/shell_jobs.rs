//! Shell jobs pane: the active conversation's background shell commands and
//! the selected job's output, following new lines while it runs.

use egui::{Color32, RichText, Ui};

use crate::model::transcript::shell_jobs::ShellJob;
use crate::theme::icons;
use crate::theme::text::muted;
use crate::theme::tokens::{FONT_SMALL, R_SM, SP_1, SP_2, palette};
use crate::theme::widgets::{compact_row, empty_state, icon_button, row_title, status_dot};

const OPEN_REQUEST: &str = "shell-jobs-open-request";

/// Asks the workbench to reveal `job_id` in the shell jobs pane.
pub fn request_open(ctx: &egui::Context, job_id: &str) {
    ctx.data_mut(|data| data.insert_temp(egui::Id::new(OPEN_REQUEST), job_id.to_owned()));
}

pub fn take_open_request(ctx: &egui::Context) -> Option<String> {
    ctx.data_mut(|data| data.remove_temp::<String>(egui::Id::new(OPEN_REQUEST)))
}

pub fn status_color(job: &ShellJob) -> Color32 {
    if job.is_running() {
        palette().RUNNING
    } else if job.failed() {
        palette().ERROR_FG
    } else {
        palette().SUCCESS
    }
}

#[derive(Debug, Clone)]
pub struct ShellJobsPane {
    selected: Option<String>,
    follow: bool,
}

impl Default for ShellJobsPane {
    fn default() -> Self {
        Self {
            selected: None,
            follow: true,
        }
    }
}

impl ShellJobsPane {
    pub fn select(&mut self, job_id: String) {
        self.selected = Some(job_id);
        self.follow = true;
    }

    /// `jobs` are the active conversation's jobs, oldest first.
    pub fn render(&mut self, ui: &mut Ui, jobs: &[&ShellJob]) {
        if jobs.is_empty() {
            empty_state(
                ui,
                "No shell jobs",
                "Background shell commands started in this conversation appear here.",
                None,
            );
            return;
        }
        // Without an explicit choice, show the newest running job, else the newest.
        let selected = self
            .selected
            .as_deref()
            .and_then(|id| jobs.iter().find(|job| job.id == id))
            .or_else(|| jobs.iter().rev().find(|job| job.is_running()))
            .or_else(|| jobs.last())
            .copied();
        let list_height = (jobs.len() as f32 * 30.0).min(ui.available_height() * 0.3);
        egui::ScrollArea::vertical()
            .id_salt("shell-jobs-list")
            .max_height(list_height)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for job in jobs.iter().rev() {
                    let current = selected.is_some_and(|selected| selected.id == job.id);
                    let mut clicked = false;
                    compact_row(ui, current, |ui| {
                        status_dot(ui, status_color(job));
                        ui.label(muted(format!("{} · {}", job.short_id(), job.outcome())));
                        clicked = row_title(ui, command_label(job)).clicked();
                    });
                    if clicked {
                        self.select(job.id.clone());
                    }
                }
            });
        ui.separator();
        if let Some(job) = selected {
            self.detail(ui, job);
        }
    }

    fn detail(&mut self, ui: &mut Ui, job: &ShellJob) {
        ui.horizontal(|ui| {
            status_dot(ui, status_color(job));
            ui.label(RichText::new(job.outcome()).color(status_color(job)));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if icon_button(ui, icons::COPY, "Copy output").clicked() {
                    ui.ctx().copy_text(job.log().to_owned());
                }
                ui.checkbox(&mut self.follow, "Follow");
            });
        });
        egui::Frame::new()
            .fill(palette().SURFACE_RAISED)
            .corner_radius(R_SM)
            .inner_margin(SP_2)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                let command = job.command.as_deref().unwrap_or("(command unavailable)");
                ui.add(egui::Label::new(RichText::new(format!("$ {command}")).monospace()).wrap());
            });
        if let Some(path) = &job.artifact {
            ui.horizontal(|ui| {
                ui.label(muted("Full output:"));
                let url = format!("file://{path}");
                ui.hyperlink_to(RichText::new(path).size(FONT_SMALL), url);
            });
        }
        if !job.is_live() {
            ui.label(muted(
                "Showing output returned to the agent; live output was not received for this job.",
            ));
        }
        if job.log_truncated() {
            ui.label(muted("Earlier output is omitted here."));
        }
        ui.add_space(SP_1);
        log_view(ui, job, self.follow);
    }
}

fn command_label(job: &ShellJob) -> String {
    job.command
        .as_deref()
        .map(|command| command.split_whitespace().collect::<Vec<_>>().join(" "))
        .unwrap_or_else(|| format!("job {}", job.short_id()))
}

fn log_view(ui: &mut Ui, job: &ShellJob, follow: bool) {
    let lines: Vec<&str> = job.log().lines().collect();
    if lines.is_empty() {
        ui.label(muted(if job.is_running() {
            "Waiting for output…"
        } else {
            "No output."
        }));
        return;
    }
    let row_height = ui.text_style_height(&egui::TextStyle::Monospace);
    egui::Frame::new()
        .fill(palette().INPUT)
        .corner_radius(R_SM)
        .inner_margin(SP_2)
        .show(ui, |ui| {
            egui::ScrollArea::both()
                .id_salt(("shell-job-log", &job.id))
                .auto_shrink([false, false])
                .stick_to_bottom(follow)
                .show_rows(ui, row_height, lines.len(), |ui, rows| {
                    for line in &lines[rows] {
                        ui.add(
                            egui::Label::new(RichText::new(*line).monospace())
                                .wrap_mode(egui::TextWrapMode::Extend),
                        );
                    }
                });
        });
}
