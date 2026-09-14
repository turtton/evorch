use super::ProviderSettingsAction;
use crate::model::provider_settings::{CodexEditorModel, ModelsFetchState, model_display_label};
use crate::theme::text::{badge, muted};
use crate::theme::tokens::palette;
use crate::theme::widgets::primary_button;

#[derive(Clone, Default)]
struct ModelInputs {
    add: String,
    error: Option<String>,
}

pub(super) fn codex_body(
    ui: &mut egui::Ui,
    editor: &mut CodexEditorModel,
) -> Option<ProviderSettingsAction> {
    let busy = editor.auth.is_authenticating();
    ui.add_enabled_ui(!busy, |ui| {
        let previous = editor.name.clone();
        let name = ui.label("Name");
        ui.add(egui::TextEdit::singleline(&mut editor.name).background_color(palette().INPUT))
            .labelled_by(name.id);
        if editor.account == previous {
            editor.account.clone_from(&editor.name);
        }
        let account = ui.label("Account");
        ui.add(egui::TextEdit::singleline(&mut editor.account).background_color(palette().INPUT))
            .labelled_by(account.id);
    });
    let login = crate::panes::codex_auth::codex_auth_section(ui, &editor.auth);
    let mut action = login.then_some(ProviderSettingsAction::StartCodexLogin);
    ui.add_enabled_ui(!busy, |ui| {
        if fetch_models(ui, editor) {
            action = Some(ProviderSettingsAction::RefreshModels);
        }
        let state_id = ui.id().with("codex-model-inputs");
        let mut inputs =
            ui.data_mut(|data| data.get_temp::<ModelInputs>(state_id).unwrap_or_default());
        ui.label(badge("Configured models"));
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
                    .background_color(palette().INPUT),
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
            ui.colored_label(palette().ERROR_FG, error);
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
    action
}

fn fetch_models(ui: &mut egui::Ui, editor: &mut CodexEditorModel) -> bool {
    let mut refresh = false;
    ui.horizontal_wrapped(|ui| {
        ui.label(badge("Fetch models"));
        refresh = ui
            .add_enabled(
                editor.fetch.models_rx.is_none(),
                egui::Button::new("Refresh models"),
            )
            .clicked();
        match &editor.fetch.models_fetch_state {
            ModelsFetchState::Idle => {}
            ModelsFetchState::Loading => {
                ui.spinner();
                ui.label(muted("Loading models…"));
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(200));
            }
            ModelsFetchState::Loaded => {
                ui.label(muted(format!(
                    "Loaded {} models from Codex",
                    editor.fetch.available_models.as_ref().map_or(0, Vec::len)
                )));
            }
            ModelsFetchState::Failed(error) => {
                ui.colored_label(
                    palette().ERROR_FG,
                    format!("Auto-fetch failed ({error}); configured models are unchanged"),
                );
            }
        }
    });
    if editor.fetch.models_fetch_state == ModelsFetchState::Loaded {
        ui.label(badge("Fetched models"));
        egui::ScrollArea::vertical()
            .id_salt("codex-fetched-models")
            .max_height(200.0)
            .show(ui, |ui| {
                if let Some(models) = &editor.fetch.available_models {
                    for id in models {
                        if editor.models.contains(id) {
                            ui.label(muted(format!("{} · Already added", model_display_label(id))));
                        } else {
                            let mut selected = editor.fetch.fetch_selected.contains(id);
                            if ui.checkbox(&mut selected, model_display_label(id)).changed() {
                                if selected {
                                    editor.fetch.fetch_selected.insert(id.clone());
                                } else {
                                    editor.fetch.fetch_selected.remove(id);
                                }
                            }
                        }
                    }
                }
            });
        ui.add_enabled_ui(!editor.fetch.fetch_selected.is_empty(), |ui| {
            if primary_button(
                ui,
                format!("Apply selected ({})", editor.fetch.fetch_selected.len()),
            )
            .clicked()
            {
                editor.apply_fetched_selection();
            }
        });
    }
    refresh
}
