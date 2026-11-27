//! The modals the Projects tab opens: registering a project and a project's settings.
//! What they change is sent as the feature's typed intents; nothing is applied here.
use egui::Ui;
use workbench_protocol::namespaces::workspace::{ProjectAccessView, TrustLevel};

use super::{Dialog, ProjectForms, ProjectIntent, Requests, WorkspaceFeature};
use crate::theme::icons;
use crate::theme::text::{h3, muted, section};
use crate::theme::tokens::{PROVIDER_MODAL_MAX_WIDTH, ROW_DENSE, SP_2, SP_4, palette};
use crate::theme::widgets::{badge, icon_button, primary_button, reason_label, surface_frame};

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
                        settings(ui, project, access, &mut self.forms, requests, &mut close);
                    }
                }
                if let Some(notice) = &self.notice {
                    reason_label(ui, palette().ERROR_FG, notice);
                }
                if let Some(confirmation) = &self.confirmation {
                    ui.label(muted(confirmation));
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
    close: &mut bool,
) {
    let approved = access.is_some_and(|access| access.trust == TrustLevel::Approved);
    ui.horizontal(|ui| {
        ui.label(h3("Project settings"));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if icon_button(ui, icons::X, "Close").clicked() {
                *close = true;
            }
            if access.is_none() {
                // Same height as the trust row it stands in for, so a read that ends does not
                // move the controls below under the pointer.
                ui.add_enabled(false, egui::Button::new("…").small());
                ui.label(muted("Trust: …"));
                return;
            }
            let (button, next) = if approved {
                ("Revoke trust", TrustLevel::Unapproved)
            } else {
                ("Approve project", TrustLevel::Approved)
            };
            if ui.small_button(button).clicked() {
                requests.trust.push((project.to_owned(), next));
            }
            let (icon, text, color) = if approved {
                (icons::SHIELD_CHECK, "Trusted", palette().SUCCESS)
            } else {
                (icons::SHIELD, "Not trusted", palette().WARNING_FG)
            };
            chip(ui, &icons::with_icon(icon, text), color);
        });
    });
    let Some(access) = access else {
        return;
    };
    ui.label(muted("Name"));
    ui.horizontal(|ui| {
        let width = (ui.available_width() - ROW_DENSE - SP_2).max(80.0);
        ui.add(
            egui::TextEdit::singleline(&mut forms.rename)
                .hint_text(access.name.as_str())
                .desired_width(width),
        );
        let name = forms.rename.trim().to_owned();
        let rename = ui
            .add_enabled_ui(!name.is_empty(), |ui| {
                icon_button(ui, icons::CHECK, "Rename project")
            })
            .inner;
        if rename.clicked() {
            requests
                .project
                .push((project.to_owned(), ProjectIntent::Rename(name)));
            forms.rename.clear();
        }
    });
    ui.label(muted(icons::with_icon(icons::FOLDER_SIMPLE, &access.root)));
    ui.label(section("ALLOWED DIRECTORIES"));
    for directory in &access.allowed_directories {
        ui.push_id(&directory.path, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(&directory.path).monospace());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if icon_button(ui, icons::TRASH, "Remove").clicked() {
                        requests.project.push((
                            project.to_owned(),
                            ProjectIntent::RemoveDirectory(directory.path.clone()),
                        ));
                    }
                    let (text, color) = if directory.trust == TrustLevel::Approved {
                        ("approved", palette().SUCCESS)
                    } else {
                        ("unapproved", palette().WARNING_FG)
                    };
                    chip(ui, text, color);
                    if !directory.live {
                        chip(ui, "missing", palette().ERROR_FG);
                    }
                });
            });
        });
    }
    ui.horizontal(|ui| {
        let width = (ui.available_width() - 160.0).max(80.0);
        ui.add(
            egui::TextEdit::singleline(&mut forms.directory)
                .hint_text("Add a directory…")
                .desired_width(width),
        );
        ui.checkbox(&mut forms.directory_approved, "approved");
        let path = forms.directory.trim().to_owned();
        let add = ui
            .add_enabled_ui(!path.is_empty(), |ui| {
                icon_button(ui, icons::PLUS, "Add directory")
            })
            .inner;
        if add.clicked() {
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

/// A small status pill: `text` in `color` on the raised surface.
fn chip(ui: &mut Ui, text: &str, color: egui::Color32) {
    badge(ui, text, color, palette().SURFACE);
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::kittest::{NodeT, Queryable};
    use workbench_protocol::{
        ids::ProjectId, namespaces::workspace::AllowedDirectoryView, wire::Revision,
    };

    fn access(trust: TrustLevel) -> ProjectAccessView {
        ProjectAccessView {
            project: ProjectId::new("p").unwrap(),
            name: "evorch".into(),
            root: "/src/evorch".into(),
            trust,
            allowed_directories: vec![
                AllowedDirectoryView {
                    path: "/cache/targets".into(),
                    trust: TrustLevel::Approved,
                    live: true,
                },
                AllowedDirectoryView {
                    path: "/gone".into(),
                    trust: TrustLevel::Unapproved,
                    live: false,
                },
            ],
            revision: Revision(1),
        }
    }

    struct Dialog {
        access: ProjectAccessView,
        forms: ProjectForms,
        requests: Requests,
        close: bool,
    }

    fn dialog(trust: TrustLevel) -> egui_kittest::Harness<'static, Dialog> {
        egui_kittest::Harness::new_ui_state(
            |ui, state: &mut Dialog| {
                settings(
                    ui,
                    "p",
                    Some(&state.access),
                    &mut state.forms,
                    &mut state.requests,
                    &mut state.close,
                );
            },
            Dialog {
                access: access(trust),
                forms: ProjectForms::default(),
                requests: Requests::default(),
                close: false,
            },
        )
    }

    // Given: a trusted project with a live and a missing allowed directory
    // When: the settings are drawn and the icons are pressed
    // Then: the trust shows as a chip with its way back, each directory carries its state and a
    //       remove icon, and each icon sends the request its text button did
    #[test]
    fn the_project_settings_show_their_state_and_keep_their_operations_as_icons() {
        let mut harness = dialog(TrustLevel::Approved);
        harness.run_steps(2);
        harness.get_by_label_contains("Trusted");
        harness.get_by_label_contains("/src/evorch");
        // The chip of the approved directory, and the add form's checkbox of the same name.
        assert_eq!(harness.get_all_by_label("approved").count(), 2);
        harness.get_by_label("unapproved");
        harness.get_by_label("missing");
        assert!(
            harness
                .query_by_label("Add directory")
                .unwrap()
                .accesskit_node()
                .is_disabled(),
            "nothing to add yet"
        );

        harness.get_by_label("Revoke trust").click();
        harness.get_all_by_label("Remove").next().unwrap().click();
        harness.run_steps(2);
        let requests = &harness.state().requests;
        assert_eq!(requests.trust, [("p".to_owned(), TrustLevel::Unapproved)]);
        assert_eq!(
            requests.project,
            [(
                "p".to_owned(),
                ProjectIntent::RemoveDirectory("/cache/targets".into())
            )]
        );

        harness.get_by_label("Close").click();
        harness.run_steps(2);
        assert!(harness.state().close);
    }

    // Given: a project that is not trusted
    // When: the settings are drawn
    // Then: the chip says so and the button offers the approval
    #[test]
    fn an_untrusted_project_offers_the_approval() {
        let mut harness = dialog(TrustLevel::Unapproved);
        harness.run_steps(2);
        harness.get_by_label_contains("Not trusted");
        harness.get_by_label("Approve project").click();
        harness.run_steps(2);
        assert_eq!(
            harness.state().requests.trust,
            [("p".to_owned(), TrustLevel::Approved)]
        );
    }
}
