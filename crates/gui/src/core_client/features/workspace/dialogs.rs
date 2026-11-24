//! The modals the Projects tab opens: registering a project and a project's settings.
//! What they change is sent as the feature's typed intents; nothing is applied here.
use egui::Ui;
use workbench_protocol::namespaces::workspace::{ProjectAccessView, TrustLevel};

use super::{Dialog, ProjectForms, ProjectIntent, Requests, WorkspaceFeature};
use crate::theme::text::{h3, muted, section};
use crate::theme::tokens::{PROVIDER_MODAL_MAX_WIDTH, SP_2, SP_4, palette};
use crate::theme::widgets::{primary_button, reason_label, surface_frame};

const NAME_LABEL: &str = "Project name";
const PATH_LABEL: &str = "Project path";

impl WorkspaceFeature {
    /// Draws the open modal, if any. Called only on a visible pass.
    pub(super) fn draw_dialog(&mut self, ctx: &egui::Context, requests: &mut Requests) {
        let Some(dialog) = self.dialog.clone() else {
            return;
        };
        let mut close = false;
        let modal = egui::Modal::new(egui::Id::new("core-project-dialog"))
            .backdrop_color(palette().OVERLAY)
            .frame(surface_frame(palette().SURFACE_RAISED))
            .show(ctx, |ui| {
                ui.set_width(
                    (ctx.viewport_rect().width() * 0.6).min(PROVIDER_MODAL_MAX_WIDTH) - SP_4 * 4.0,
                );
                ui.spacing_mut().item_spacing = egui::vec2(SP_2, SP_2);
                match &dialog {
                    Dialog::AddProject => {
                        add_project(ui, &mut self.forms, requests, &mut close);
                    }
                    Dialog::Settings(project) => {
                        let access = self.model.access.get(project);
                        settings(ui, project, access, &mut self.forms, requests);
                    }
                }
                if let Some(notice) = &self.notice {
                    reason_label(ui, palette().ERROR_FG, notice);
                }
                if let Some(confirmation) = &self.confirmation {
                    ui.label(muted(confirmation));
                }
                if matches!(dialog, Dialog::Settings(_)) && ui.button("Close").clicked() {
                    close = true;
                }
            });
        if close || modal.should_close() {
            self.dialog = None;
        }
    }
}

fn field(ui: &mut Ui, text: &mut String, label: &str) -> egui::Response {
    let response = ui.add(
        egui::TextEdit::singleline(text)
            .hint_text(label)
            .desired_width(ui.available_width()),
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, label));
    response
}

fn add_project(ui: &mut Ui, forms: &mut ProjectForms, requests: &mut Requests, close: &mut bool) {
    ui.label(h3("New project"));
    field(ui, &mut forms.register_name, NAME_LABEL);
    let path = field(ui, &mut forms.register_path, PATH_LABEL);
    let submitted = path.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
    let ready = !forms.register_name.trim().is_empty() && !forms.register_path.trim().is_empty();
    ui.horizontal(|ui| {
        let add = ui
            .add_enabled_ui(ready, |ui| primary_button(ui, "Add"))
            .inner;
        if (add.clicked() || submitted) && ready {
            requests.register = Some((forms.register_name.clone(), forms.register_path.clone()));
        }
        if ui.button("Cancel").clicked() {
            *close = true;
        }
    });
}

fn settings(
    ui: &mut Ui,
    project: &str,
    access: Option<&ProjectAccessView>,
    forms: &mut ProjectForms,
    requests: &mut Requests,
) {
    ui.label(h3("Project settings"));
    let Some(access) = access else {
        // Same height as the trust row it stands in for, so a read that ends does not
        // move the controls below under the pointer.
        ui.horizontal(|ui| {
            ui.label("Trust: …");
            ui.add_enabled(false, egui::Button::new("…").small());
        });
        return;
    };
    ui.label(format!("Directory: {}", access.root));
    let approved = access.trust == TrustLevel::Approved;
    ui.horizontal(|ui| {
        ui.label(if approved {
            "Trust: Approved"
        } else {
            "Trust: Unapproved"
        });
        let (button, next) = if approved {
            ("Revoke trust", TrustLevel::Unapproved)
        } else {
            ("Approve project", TrustLevel::Approved)
        };
        if ui.small_button(button).clicked() {
            requests.trust.push((project.to_owned(), next));
        }
    });
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(&mut forms.rename).hint_text(access.name.as_str()));
        let name = forms.rename.trim().to_owned();
        if ui
            .add_enabled(!name.is_empty(), egui::Button::new("Rename project"))
            .clicked()
        {
            requests
                .project
                .push((project.to_owned(), ProjectIntent::Rename(name)));
            forms.rename.clear();
        }
    });
    ui.label(section("Allowed directories"));
    for directory in &access.allowed_directories {
        ui.push_id(&directory.path, |ui| {
            ui.horizontal(|ui| {
                ui.label(format!(
                    "{} ({}{})",
                    directory.path,
                    if directory.trust == TrustLevel::Approved {
                        "approved"
                    } else {
                        "unapproved"
                    },
                    if directory.live { "" } else { ", missing" }
                ));
                if ui.small_button("Remove").clicked() {
                    requests.project.push((
                        project.to_owned(),
                        ProjectIntent::RemoveDirectory(directory.path.clone()),
                    ));
                }
            });
        });
    }
    ui.horizontal(|ui| {
        ui.text_edit_singleline(&mut forms.directory);
        ui.checkbox(&mut forms.directory_approved, "Approved");
        let path = forms.directory.trim().to_owned();
        if ui
            .add_enabled(!path.is_empty(), egui::Button::new("Add directory"))
            .clicked()
        {
            let trust = if forms.directory_approved {
                TrustLevel::Approved
            } else {
                TrustLevel::Unapproved
            };
            requests.project.push((
                project.to_owned(),
                ProjectIntent::AddDirectory { path, trust },
            ));
            forms.directory.clear();
        }
    });
}
