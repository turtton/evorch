use std::collections::BTreeMap;
use std::path::PathBuf;

use workspace_ui::{ProjectId, SidebarState, ThreadId, ThreadRunPhase, ThreadState, TrustState};

const UI_STATE_ID: &str = "sidebar-ui-state";

#[derive(Clone, Default)]
struct SidebarUiState {
    project_path: String,
    error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidebarAction {
    SelectProject(ProjectId),
    AddProject(PathBuf),
    CreateThread(String),
    SwitchThread(ThreadId),
    TogglePin(ThreadId),
    TogglePause(ThreadId),
    SetTrust { path: PathBuf, trust: TrustState },
}

pub fn sidebar_pane(
    ui: &mut egui::Ui,
    sidebar: &SidebarState,
    phases: &BTreeMap<String, ThreadRunPhase>,
) -> Option<SidebarAction> {
    let id = egui::Id::new(UI_STATE_ID);
    let mut pane_state = ui
        .ctx()
        .data_mut(|data| data.get_temp::<SidebarUiState>(id))
        .unwrap_or_default();
    let mut action = None;

    ui.heading("Projects");
    for project in &sidebar.projects {
        let selected = sidebar.selected_project.as_ref() == Some(&project.id);
        if ui.selectable_label(selected, &project.name).clicked() {
            action = Some(SidebarAction::SelectProject(project.id.clone()));
        }
    }

    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut pane_state.project_path)
                .hint_text("Project path")
                .desired_width(140.0),
        );
        if ui.button("Add project").clicked() && !pane_state.project_path.trim().is_empty() {
            action = Some(SidebarAction::AddProject(PathBuf::from(
                pane_state.project_path.trim(),
            )));
            pane_state.project_path.clear();
        }
    });

    if let Some(error) = &pane_state.error {
        ui.colored_label(ui.visuals().error_fg_color, error);
    }

    if let Some(project) = selected_project(sidebar) {
        ui.separator();
        ui.label("Allowed directories");
        for directory in &project.allowed_directories {
            ui.label(directory.path.display().to_string());
            ui.horizontal(|ui| match directory.trust {
                TrustState::Approved => {
                    ui.label("trusted");
                }
                TrustState::Unapproved => {
                    ui.label("untrusted");
                    if ui.button("Trust").clicked() {
                        action = Some(SidebarAction::SetTrust {
                            path: directory.path.clone(),
                            trust: TrustState::Approved,
                        });
                    }
                }
            });
        }

        ui.separator();
        ui.horizontal(|ui| {
            ui.label("Threads");
            if ui.button("New thread").clicked() {
                let title = format!("thread-{}", sidebar.threads.len() + 1);
                action = Some(SidebarAction::CreateThread(title));
            }
        });

        for thread in sidebar
            .threads
            .iter()
            .filter(|thread| thread.project_id == project.id)
        {
            ui.horizontal(|ui| {
                let pin = if thread.pinned { "★" } else { "☆" };
                if ui.button(pin).clicked() {
                    action = Some(SidebarAction::TogglePin(thread.id.clone()));
                }
                let active = sidebar.active_thread.as_ref() == Some(&thread.id);
                if ui.selectable_label(active, &thread.title).clicked() {
                    action = Some(SidebarAction::SwitchThread(thread.id.clone()));
                }
                ui.label(thread_state_label(thread.state(phases)));
                let pause = if thread.paused { "Resume" } else { "Pause" };
                if ui.button(pause).clicked() {
                    action = Some(SidebarAction::TogglePause(thread.id.clone()));
                }
            });
            if let (Some(branch), Some(worktree)) = (&thread.branch, &thread.worktree_path) {
                ui.label(format!("{branch} @ {}", worktree.display()));
            }
        }
    }

    ui.ctx().data_mut(|data| data.insert_temp(id, pane_state));
    action
}

pub fn set_sidebar_error(ctx: &egui::Context, error: Option<String>) {
    let id = egui::Id::new(UI_STATE_ID);
    ctx.data_mut(|data| {
        let state = data.get_temp_mut_or_default::<SidebarUiState>(id);
        state.error = error;
    });
}

fn selected_project(sidebar: &SidebarState) -> Option<&workspace_ui::ProjectRecord> {
    let selected = sidebar.selected_project.as_ref()?;
    sidebar
        .projects
        .iter()
        .find(|project| &project.id == selected)
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
