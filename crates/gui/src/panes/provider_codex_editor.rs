use crate::model::provider_settings::CodexEditorModel;
use crate::theme::text::{h3, muted};
use crate::theme::tokens::{ERROR_FG, INPUT};

#[derive(Clone, Default)]
struct ModelInputs {
    add: String,
    error: Option<String>,
}

pub(super) fn codex_body(ui: &mut egui::Ui, editor: &mut CodexEditorModel) -> bool {
    let busy = editor.auth.is_authenticating();
    ui.add_enabled_ui(!busy, |ui| {
        let previous = editor.name.clone();
        let name = ui.label("Name");
        ui.add(egui::TextEdit::singleline(&mut editor.name).background_color(INPUT))
            .labelled_by(name.id);
        if editor.account == previous {
            editor.account.clone_from(&editor.name);
        }
        let account = ui.label("Account");
        ui.add(egui::TextEdit::singleline(&mut editor.account).background_color(INPUT))
            .labelled_by(account.id);
    });
    let login = crate::panes::codex_auth::codex_auth_section(ui, &editor.auth);
    ui.add_enabled_ui(!busy, |ui| {
        let state_id = ui.id().with("codex-model-inputs");
        let mut inputs =
            ui.data_mut(|data| data.get_temp::<ModelInputs>(state_id).unwrap_or_default());
        ui.label(h3("Configured models"));
        let mut remove = None;
        for (index, id) in editor.models.iter().enumerate() {
            ui.push_id(id, |ui| {
                ui.horizontal(|ui| {
                    ui.add_sized(
                        [(ui.available_width() - 80.0).max(40.0), 20.0],
                        egui::Label::new(id).truncate(),
                    )
                    .on_hover_text(id);
                    let button = ui.button("Remove");
                    button.widget_info(|| {
                        egui::WidgetInfo::labeled(
                            egui::WidgetType::Button,
                            ui.is_enabled(),
                            format!("Remove {id}"),
                        )
                    });
                    if button.clicked() {
                        remove = Some(index);
                    }
                });
            });
        }
        if let Some(index) = remove {
            let removed = editor.models.remove(index);
            if editor.default_model == removed {
                editor.default_model = editor.models.first().cloned().unwrap_or_default();
            }
            inputs.error = None;
        }
        let label = ui.label("Add model");
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut inputs.add)
                    .desired_width((ui.available_width() - 60.0).max(40.0))
                    .background_color(INPUT),
            )
            .labelled_by(label.id);
            if ui.button("Add").clicked() {
                let id = inputs.add.trim();
                inputs.error = None;
                if !id.is_empty() {
                    if editor.models.iter().any(|model| model == id) {
                        inputs.error = Some("Model already added".into());
                    } else {
                        if editor.default_model.is_empty() {
                            editor.default_model = id.to_owned();
                        }
                        editor.models.push(id.to_owned());
                        inputs.add.clear();
                    }
                }
            }
        });
        if let Some(error) = &inputs.error {
            ui.colored_label(ERROR_FG, error);
        }
        let label = ui.label("Default model");
        egui::ComboBox::from_id_salt("codex-default-model")
            .width(ui.available_width())
            .selected_text(&editor.default_model)
            .show_ui(ui, |ui| {
                for id in &editor.models {
                    ui.selectable_value(&mut editor.default_model, id.clone(), id);
                }
            })
            .response
            .labelled_by(label.id);
        ui.add(egui::Label::new(muted(
            "Used when a route doesn't override the model and when re-resolving a pinned session.",
        )).wrap());
        ui.data_mut(|data| data.insert_temp(state_id, inputs));
    });
    login
}
