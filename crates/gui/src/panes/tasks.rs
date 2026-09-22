//! Work-centered task progress, dependencies, and team assignments.

use std::collections::BTreeMap;

use runtime::{RunId, team::TeamTask};
use storage::{StorageConfig, entity::TaskRecord, task_queue::TaskDependencies};

use crate::model::durable_tasks::{DurableTaskRow, DurableTasksModel};
use crate::theme::{
    text::{h3, muted},
    tokens::{SP_2, palette},
    widgets::{badge, empty_state, pane_root, surface_frame},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TasksAction {
    OpenRun(String),
}

/// Tasks retain their identity across executions; team claims use their own scope.
pub fn tasks_pane(
    ui: &mut egui::Ui,
    model: &DurableTasksModel,
    teams: &[(RunId, Vec<TeamTask>)],
    config: Option<&StorageConfig>,
    selected_task: Option<&str>,
) -> Option<TasksAction> {
    let stored = config.map(load_tasks).transpose();
    let mut action = None;
    if selected_task.is_none() {
        ui.ctx().data_mut(|data| {
            data.remove::<String>(egui::Id::new("tasks_scrolled_selection"));
        });
    }
    pane_root(ui, "Tasks", |ui| {
        egui::ScrollArea::vertical()
            .id_salt("tasks_scroll")
            .show(ui, |ui| {
                ui.label(h3("Task progress"));
                let mut rows: BTreeMap<_, _> = model
                    .rows()
                    .map(|row| (row.id.clone(), row.clone()))
                    .collect();
                let mut dependencies = BTreeMap::new();
                match stored {
                    Ok(Some(stored)) => {
                        for (task, links) in stored {
                            dependencies.insert(task.id.clone(), links);
                            // TaskRecord and TaskProgressed share the durable task ID.
                            rows.entry(task.id.clone())
                                .or_insert_with(|| stored_row(task));
                        }
                    }
                    Err(error) => {
                        ui.colored_label(palette().ERROR_FG, format!("Cannot load tasks: {error}"));
                    }
                    Ok(None) => {}
                }
                if rows.is_empty() && teams.iter().all(|(_, tasks)| tasks.is_empty()) {
                    empty_state(
                        ui,
                        "No tasks yet",
                        "Tasks and goals show progress, retries, artifacts, and related runs here.",
                        None,
                    );
                } else {
                    for row in rows.values() {
                        ui.push_id(&row.id, |ui| {
                            let fill = if selected_task == Some(row.id.as_str()) {
                                palette().ACTIVE_ROW
                            } else {
                                palette().SURFACE
                            };
                            surface_frame(fill).show(ui, |ui| {
                                ui.set_min_width(ui.available_width());
                                ui.horizontal_wrapped(|ui| {
                                    task_label(ui, &row.id, &row.title, &row.id, selected_task);
                                    ui.label(row.status.as_str());
                                    if row.attempt > 0 {
                                        badge(
                                            ui,
                                            format!("retry {}", row.attempt),
                                            palette().TEXT,
                                            palette().SURFACE,
                                        );
                                    }
                                });
                                if row.title != row.id {
                                    ui.add(egui::Label::new(&row.title).wrap());
                                }
                                if !row.detail.is_empty() {
                                    ui.label(muted("Progress").strong());
                                    ui.add(egui::Label::new(&row.detail).wrap());
                                }
                                ui.label(muted("Last valid artifact").strong());
                                ui.add(
                                    egui::Label::new(row.last_artifact.as_deref().unwrap_or("-"))
                                        .wrap(),
                                );
                                ui.horizontal_wrapped(|ui| {
                                    ui.label(muted("Related runs").strong());
                                    if row.run_ids.is_empty() {
                                        ui.label(muted("-"));
                                    }
                                    for run in &row.run_ids {
                                        let current = row.run_id.as_ref() == Some(run);
                                        if ui
                                            .link(run)
                                            .on_hover_text(if current {
                                                "Open current execution"
                                            } else {
                                                "Open related execution"
                                            })
                                            .clicked()
                                        {
                                            action = Some(TasksAction::OpenRun(run.clone()));
                                        }
                                        if current {
                                            ui.label(muted("(current)"));
                                        }
                                    }
                                });
                                if let Some(links) = dependencies.get(&row.id) {
                                    if !links.blocks.is_empty() {
                                        ui.horizontal_wrapped(|ui| {
                                            ui.label(muted("Blocks").strong());
                                            dependency_labels(ui, &links.blocks);
                                        });
                                    }
                                    if !links.blocked_by.is_empty() {
                                        ui.horizontal_wrapped(|ui| {
                                            ui.label(muted("Blocked by").strong());
                                            dependency_labels(ui, &links.blocked_by);
                                        });
                                    }
                                }
                            });
                            ui.add_space(SP_2);
                        });
                    }
                }
                ui.add_space(SP_2);
                ui.separator();
                if let Some(team_action) =
                    crate::panes::team::team_tasks_pane(ui, teams, selected_task)
                {
                    action = Some(team_action);
                }
            });
    });
    action
}

fn load_tasks(
    config: &StorageConfig,
) -> Result<Vec<(TaskRecord, TaskDependencies)>, storage::StorageError> {
    let db = storage::Database::open(config)?;
    db.durable_tasks()?
        .into_iter()
        .map(|task| {
            let links = db.task_dependencies(&task.id)?;
            Ok((task, links))
        })
        .collect()
}

fn stored_row(task: TaskRecord) -> DurableTaskRow {
    DurableTaskRow {
        title: task.id.clone(),
        id: task.id,
        // parent_run_id identifies the parent, not the assigned execution.
        run_id: None,
        run_ids: Vec::new(),
        goal_id: None,
        status: task.status,
        attempt: task.attempts,
        last_artifact: task.last_artifact,
        detail: task.failure_reason.unwrap_or_default(),
    }
}

fn dependency_labels(ui: &mut egui::Ui, dependencies: &[String]) {
    if dependencies.is_empty() {
        ui.label(muted("-"));
    } else {
        for dependency in dependencies {
            ui.monospace(dependency);
        }
    }
}

pub(super) fn task_label(
    ui: &mut egui::Ui,
    id: &str,
    title: &str,
    selection_key: &str,
    selected_task: Option<&str>,
) {
    let selected = selected_task == Some(selection_key);
    let response = ui
        .add(egui::Label::new(egui::RichText::new(id).strong().color(palette().TEXT)).wrap())
        .on_hover_text(title);
    let memory_id = egui::Id::new("tasks_scrolled_selection");
    if selected
        && ui
            .ctx()
            .data(|data| data.get_temp::<String>(memory_id))
            .as_deref()
            != Some(selection_key)
    {
        response.scroll_to_me(Some(egui::Align::Center));
        ui.ctx()
            .data_mut(|data| data.insert_temp(memory_id, selection_key.to_owned()));
    }
}
