use std::collections::BTreeMap;

use egui::{Sense, Ui};
use workspace_ui::{ProjectRecord, SidebarState, ThreadRunPhase, ThreadState};

use crate::theme::text::h4;
use crate::theme::tokens::state_color;
use crate::theme::tokens::{ROW_COMPACT, SP_2};
use crate::theme::widgets::{compact_row, empty_state, primary_button, status_dot};

use super::{SidebarAction, SidebarUiState};

const THREAD_ROW_RIGHT_WIDTH: f32 = 140.0;

pub fn render(
    ui: &mut Ui,
    sidebar: &SidebarState,
    project: &ProjectRecord,
    phases: &BTreeMap<String, ThreadRunPhase>,
    _pane_state: &mut SidebarUiState,
    action: &mut Option<SidebarAction>,
) {
    let project_threads: Vec<_> = sidebar
        .threads
        .iter()
        .filter(|thread| thread.project_id == project.id)
        .collect();

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
            let pin = if thread.pinned { "★" } else { "☆" };
            if ui.button(pin).clicked() {
                *action = Some(SidebarAction::TogglePin(thread.id.clone()));
            }
            status_dot(ui, state_color(state));
            let title_width = (ui.available_width() - THREAD_ROW_RIGHT_WIDTH).max(40.0);
            let title_response = ui.add_sized(
                egui::vec2(title_width, ROW_COMPACT),
                egui::Label::new(&thread.title)
                    .truncate()
                    .sense(Sense::click()),
            );
            if title_response.clicked() {
                *action = Some(SidebarAction::SwitchThread(thread.id.clone()));
            }
            ui.label(thread_state_label(state));
            let pause = if thread.paused { "Resume" } else { "Pause" };
            if ui.button(pause).clicked() {
                *action = Some(SidebarAction::TogglePause(thread.id.clone()));
            }
        });
        if let (Some(branch), Some(worktree)) = (&thread.branch, &thread.worktree_path) {
            ui.label(crate::theme::text::muted(format!(
                "{branch} @ {}",
                worktree.display()
            )));
        }
    }

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
