use std::collections::BTreeMap;

use egui::{Align, Layout, Sense, Ui};
use workspace_ui::{ProjectRecord, SidebarState, ThreadRecord, ThreadRunPhase, ThreadState};

use crate::model::telemetry::TelemetryOverlay;
use crate::theme::text::h4;
use crate::theme::tokens::state_color;
use crate::theme::tokens::{ROW_DENSE, SP_1, SP_2};
use crate::theme::widgets::{compact_row, empty_state, primary_button, status_dot};

use super::{SidebarAction, SidebarUiState};

pub fn render(
    ui: &mut Ui,
    sidebar: &SidebarState,
    project: &ProjectRecord,
    phases: &BTreeMap<String, ThreadRunPhase>,
    _telemetry: &TelemetryOverlay,
    _pane_state: &mut SidebarUiState,
    action: &mut Option<SidebarAction>,
) {
    let (project_threads, archived) =
        ThreadRecord::partition_for_project(&sidebar.threads, &project.id);

    ui.separator();
    ui.horizontal(|ui| {
        ui.label(h4("Threads"));
        let has_threads = !project_threads.is_empty();
        let new_thread_clicked = if has_threads {
            ui.button("New thread").clicked()
        } else {
            primary_button(ui, "New thread").clicked()
        };
        if new_thread_clicked {
            let title = format!("thread-{}", sidebar.threads.len() + 1);
            *action = Some(SidebarAction::CreateThread(title));
        }
    });

    if project_threads.is_empty() {
        empty_state(
            ui,
            "No threads yet",
            "Start a thread to begin a conversation.",
            None,
        );
    }

    for thread in project_threads {
        let active = sidebar.active_thread.as_ref() == Some(&thread.id);
        let state = thread.state(phases);
        compact_row(ui, active, |ui| {
            ui.spacing_mut().item_spacing.x = SP_1;
            ui.spacing_mut().button_padding.x = SP_1;
            let pin = if thread.pinned { "★" } else { "☆" };
            if ui.button(pin).clicked() {
                *action = Some(SidebarAction::TogglePin(thread.id.clone()));
            }
            status_dot(ui, state_color(state));
            let archive = ui
                .add_enabled(!thread.pinned, egui::Button::new("▣").small())
                .on_hover_text("アーカイブ");
            archive.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Archive")
            });
            if archive.clicked() {
                *action = Some(SidebarAction::ToggleArchive(thread.id.clone()));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let pause = if thread.paused { "Resume" } else { "Pause" };
                if ui.button(pause).clicked() {
                    *action = Some(SidebarAction::TogglePause(thread.id.clone()));
                }
                if ui.small_button("Fork").clicked() {
                    *action = Some(SidebarAction::ForkThread(thread.id.clone()));
                }
                ui.label(thread_state_label(state));
                let title_response = ui.add_sized(
                    egui::vec2(ui.available_width().max(0.0), ROW_DENSE),
                    egui::Label::new(format!(
                        "{}{}",
                        if thread.parent_thread_id.is_some() {
                            "↳ "
                        } else {
                            ""
                        },
                        thread.title
                    ))
                    .truncate()
                    .halign(Align::LEFT)
                    .sense(Sense::click()),
                );
                if title_response.clicked() {
                    *action = Some(SidebarAction::SwitchThread(thread.id.clone()));
                }
            });
        });
        if let (Some(branch), Some(worktree)) = (&thread.branch, &thread.worktree_path) {
            ui.label(crate::theme::text::muted(format!(
                "{branch} @ {}",
                worktree.display()
            )));
        }
    }

    egui::CollapsingHeader::new(format!("アーカイブ済み ({})", archived.len()))
        .id_salt(("archived-threads", &project.id))
        .show(ui, |ui| {
            for thread in archived {
                let active = sidebar.active_thread.as_ref() == Some(&thread.id);
                compact_row(ui, active, |ui| {
                    if ui
                        .small_button("Restore")
                        .on_hover_text("アーカイブを解除")
                        .clicked()
                    {
                        *action = Some(SidebarAction::ToggleArchive(thread.id.clone()));
                    }
                    if ui
                        .add_sized(
                            egui::vec2(ui.available_width().max(0.0), ROW_DENSE),
                            egui::Label::new(&thread.title)
                                .truncate()
                                .halign(Align::LEFT)
                                .sense(Sense::click()),
                        )
                        .clicked()
                    {
                        *action = Some(SidebarAction::SwitchThread(thread.id.clone()));
                    }
                });
            }
        });
    ui.add_space(SP_2);
}

const fn thread_state_label(state: ThreadState) -> &'static str {
    match state {
        ThreadState::Active => "Active",
        ThreadState::Paused => "Paused",
        ThreadState::Running => "Running",
        ThreadState::Waiting => "Waiting",
        ThreadState::Done => "Done",
        ThreadState::Error => "Error",
    }
}
