//! Modals opened from the Projects sidebar.

use std::path::PathBuf;

use egui::{Align, Layout, Sense, Ui};
use workspace_ui::{ProjectId, ProjectRecord, TrustState};

use crate::model::project_dialog::ProjectDialog;
use crate::theme::icons;
use crate::theme::text::{h3, muted, section};
use crate::theme::tokens::*;
use crate::theme::widgets::{badge, fill_label, ghost, primary_button, surface_frame};

pub const PATH_LABEL: &str = "Project path (~ allowed)";
pub const NAME_LABEL: &str = "Project name";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectDialogAction {
    Browse,
    Add(PathBuf),
    Rename {
        project: ProjectId,
        name: String,
    },
    SetTrust {
        project: ProjectId,
        path: PathBuf,
        trust: TrustState,
    },
    Close,
}

pub fn project_dialog_modal(
    ctx: &egui::Context,
    dialog: &mut ProjectDialog,
    projects: &[ProjectRecord],
    picker_busy: bool,
) -> Option<ProjectDialogAction> {
    let mut action = None;
    let modal = egui::Modal::new(egui::Id::new("project-dialog"))
        .backdrop_color(palette().OVERLAY)
        .frame(surface_frame(palette().SURFACE_RAISED))
        .show(ctx, |ui| {
            ui.set_width(
                (ctx.viewport_rect().width() * 0.6).min(PROVIDER_MODAL_MAX_WIDTH) - SP_4 * 4.0,
            );
            ui.spacing_mut().item_spacing = egui::vec2(SP_2, SP_2);
            match dialog {
                ProjectDialog::Closed => {}
                ProjectDialog::Add { path, error } => {
                    add_project(ui, path, error.as_deref(), picker_busy, &mut action);
                }
                ProjectDialog::Settings {
                    project,
                    name,
                    error,
                } => match projects.iter().find(|record| &record.id == project) {
                    Some(project) => {
                        project_settings(ui, project, name, error.as_deref(), &mut action);
                    }
                    // The project disappeared underneath the modal.
                    None => action = Some(ProjectDialogAction::Close),
                },
            }
        });
    if action.is_none() && modal.should_close() {
        action = Some(ProjectDialogAction::Close);
    }
    action
}

fn add_project(
    ui: &mut Ui,
    path: &mut String,
    error: Option<&str>,
    picker_busy: bool,
    action: &mut Option<ProjectDialogAction>,
) {
    ui.label(h3("New project"));
    let submitted = ui
        .horizontal(|ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let browse = ui.add_enabled(
                    !picker_busy,
                    ghost(icons::with_icon(icons::FOLDER_OPEN, "Browse…")),
                );
                browse.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::WidgetType::Button, !picker_busy, "Browse…")
                });
                if browse.clicked() {
                    *action = Some(ProjectDialogAction::Browse);
                }
                let input = ui.add(
                    egui::TextEdit::singleline(path)
                        .hint_text(PATH_LABEL)
                        .desired_width(ui.available_width()),
                );
                input.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, PATH_LABEL)
                });
                input.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter))
            })
            .inner
        })
        .inner;
    if let Some(error) = error {
        ui.colored_label(palette().ERROR_FG, error);
    }
    let trimmed = path.trim();
    ui.horizontal(|ui| {
        let add = ui
            .add_enabled_ui(!trimmed.is_empty(), |ui| primary_button(ui, "Add"))
            .inner;
        if (add.clicked() || submitted) && !trimmed.is_empty() {
            *action = Some(ProjectDialogAction::Add(PathBuf::from(trimmed)));
        }
        if ui.button("Cancel").clicked() {
            *action = Some(ProjectDialogAction::Close);
        }
    });
}

fn project_settings(
    ui: &mut Ui,
    project: &ProjectRecord,
    name: &mut String,
    error: Option<&str>,
    action: &mut Option<ProjectDialogAction>,
) {
    ui.label(h3("Project settings"));

    ui.label(section("Name"));
    let renamable = !name.trim().is_empty() && name.trim() != project.name;
    ui.horizontal(|ui| {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let rename = ui
                .add_enabled_ui(renamable, |ui| primary_button(ui, "Rename"))
                .inner;
            let input =
                ui.add(egui::TextEdit::singleline(name).desired_width(ui.available_width()));
            input.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, NAME_LABEL)
            });
            let submitted =
                input.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
            if (rename.clicked() || submitted) && renamable {
                *action = Some(ProjectDialogAction::Rename {
                    project: project.id.clone(),
                    name: name.trim().to_owned(),
                });
            }
        });
    });

    ui.label(section("Path"));
    copyable_path(ui, &project.repo_root.display().to_string());

    ui.label(section("Allowed directories"));
    if project.allowed_directories.is_empty() {
        ui.label(muted("No directories outside the project root."));
    }
    for directory in &project.allowed_directories {
        ui.push_id(&directory.path, |ui| {
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    match directory.trust {
                        TrustState::Approved => {
                            badge(ui, "trusted", palette().TEXT_MUTED, palette().SURFACE);
                        }
                        TrustState::Unapproved => {
                            if ui.button("Trust").clicked() {
                                *action = Some(ProjectDialogAction::SetTrust {
                                    project: project.id.clone(),
                                    path: directory.path.clone(),
                                    trust: TrustState::Approved,
                                });
                            }
                            badge(ui, "untrusted", palette().WARNING, palette().SURFACE);
                        }
                    }
                    copyable_path(ui, &directory.path.display().to_string());
                });
            });
        });
    }

    if let Some(error) = error {
        ui.colored_label(palette().ERROR_FG, error);
    }
    ui.add_space(SP_2);
    if ui.button("Close").clicked() {
        *action = Some(ProjectDialogAction::Close);
    }
}

fn copyable_path(ui: &mut Ui, path: &str) {
    if fill_label(ui, path, ROW_DENSE, Sense::click())
        .on_hover_text(format!("{path}\nClick to copy"))
        .clicked()
    {
        ui.ctx().copy_text(path.to_owned());
    }
}
