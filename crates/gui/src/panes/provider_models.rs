use crate::model::provider_settings::{ModelsFetchState, OpenAiEditorModel};
use crate::theme::text::{h3, muted};
use crate::theme::tokens::{ERROR_FG, INPUT};
use crate::theme::widgets::primary_button;

#[derive(Clone, Default)]
struct ModelInputs {
    add: String,
    editing: Option<(String, String)>,
    error: Option<String>,
    height: Option<f32>,
}

pub fn provider_models(
    ui: &mut egui::Ui,
    editor: &mut OpenAiEditorModel,
    sources: &crate::model::model_metadata::MetadataSources<'_>,
) -> bool {
    let top = ui.cursor().top();
    let state_id = ui.id().with("provider-model-inputs");
    let mut inputs = ui.data_mut(|data| data.get_temp::<ModelInputs>(state_id).unwrap_or_default());
    ui.label(h3("Configured models"));
    egui::ScrollArea::vertical()
        .id_salt("configured-models")
        .max_height(120.0)
        .show(ui, |ui| {
            let mut remove = None;
            for index in 0..editor.models.len() {
                let id = editor.models[index].id.clone();
                ui.push_id(&id, |ui| {
                    ui.horizontal(|ui| {
                        let mut enabled = editor.models[index].enabled;
                        let toggle = ui.checkbox(&mut enabled, "");
                        toggle.widget_info(|| {
                            egui::WidgetInfo::selected(
                                egui::WidgetType::Checkbox,
                                ui.is_enabled(),
                                enabled,
                                format!("Enable {id}"),
                            )
                        });
                        if toggle.changed() {
                            inputs.error = editor.set_model_enabled(index, enabled).err();
                        }
                        let editing = inputs
                            .editing
                            .as_ref()
                            .is_some_and(|(original, _)| original == &id);
                        if editing {
                            if let Some((_, draft)) = &mut inputs.editing {
                                let edit = ui.add(
                                    egui::TextEdit::singleline(draft)
                                        .desired_width((ui.available_width() - 130.0).max(40.0))
                                        .background_color(INPUT),
                                );
                                edit.widget_info(|| {
                                    egui::WidgetInfo::labeled(
                                        egui::WidgetType::TextEdit,
                                        ui.is_enabled(),
                                        format!("Model ID {id}"),
                                    )
                                });
                                let done = ui.button("Done");
                                done.widget_info(|| {
                                    egui::WidgetInfo::labeled(
                                        egui::WidgetType::Button,
                                        ui.is_enabled(),
                                        format!("Done {id}"),
                                    )
                                });
                                if done.clicked()
                                    || (edit.lost_focus()
                                        && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                                {
                                    inputs.error = editor.rename_model(index, draft).err();
                                    if inputs.error.is_none() {
                                        inputs.editing = None;
                                    }
                                }
                            }
                        } else {
                            ui.add_sized(
                                [(ui.available_width() - 130.0).max(40.0), 20.0],
                                egui::Label::new(&id).truncate(),
                            )
                            .on_hover_text(&id);
                            let edit = ui.button("Edit");
                            edit.widget_info(|| {
                                egui::WidgetInfo::labeled(
                                    egui::WidgetType::Button,
                                    ui.is_enabled(),
                                    format!("Edit {id}"),
                                )
                            });
                            if edit.clicked() {
                                inputs.editing = Some((id.clone(), id.clone()));
                                inputs.error = None;
                            }
                        }
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
                    ui.collapsing(format!("Metadata: {id}"), |ui| {
                        super::model_metadata::model_metadata(
                            ui,
                            &mut editor.models[index],
                            sources,
                        );
                    });
                    ui.horizontal_wrapped(|ui| {
                        for label in sources.labels(&editor.models[index], &editor.name) {
                            ui.label(muted(label));
                        }
                    });
                });
            }
            if let Some(index) = remove {
                inputs.error = editor.remove_model(index).err();
                inputs.editing = None;
            }
        });
    let label = ui.label("Add model");
    ui.horizontal(|ui| {
        let input = ui
            .add(
                egui::TextEdit::singleline(&mut inputs.add)
                    .desired_width((ui.available_width() - 60.0).max(40.0))
                    .background_color(INPUT),
            )
            .labelled_by(label.id);
        let enter = input.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if ui.button("Add").clicked() || enter {
            inputs.error = editor.add_model(&inputs.add).err();
            if inputs.error.is_none() {
                inputs.add.clear();
            }
            if enter {
                input.request_focus();
            }
        }
    });
    if let Some(error) = &inputs.error {
        ui.colored_label(ERROR_FG, error);
    }
    if let Some(error) = &editor.validation_error {
        ui.colored_label(ERROR_FG, error);
    }
    let refresh = fetch_models(ui, editor);
    let height = ui.cursor().top() - top;
    if inputs.height != Some(height) {
        ui.ctx().request_repaint();
        inputs.height = Some(height);
    }
    ui.data_mut(|data| data.insert_temp(state_id, inputs));
    refresh
}

fn fetch_models(ui: &mut egui::Ui, editor: &mut OpenAiEditorModel) -> bool {
    let mut refresh = false;
    ui.horizontal_wrapped(|ui| {
        ui.label("Fetch models");
        refresh = ui.button("Refresh models").clicked();
        match &editor.models_fetch_state {
            ModelsFetchState::Idle => {}
            ModelsFetchState::Loading => {
                ui.spinner();
                ui.label(muted("Loading models…"));
            }
            ModelsFetchState::Loaded => {
                ui.label(muted(format!(
                    "Loaded {} models from /v1/models",
                    editor.available_models.as_ref().map_or(0, Vec::len)
                )));
            }
            ModelsFetchState::Failed(error) => {
                ui.colored_label(
                    ERROR_FG,
                    format!("Auto-fetch failed ({error}); manual entry below"),
                );
            }
        }
    });
    if editor.models_fetch_state == ModelsFetchState::Loaded {
        ui.label(h3("Fetched models"));
        let mut toggled = None;
        egui::ScrollArea::vertical()
            .id_salt("fetched-models")
            .max_height(100.0)
            .show(ui, |ui| {
                if let Some(models) = &editor.available_models {
                    for id in models {
                        if editor.is_added(id) {
                            ui.label(muted(format!("{id} · Already added")));
                        } else {
                            let mut selected = editor.fetch_selected.contains(id);
                            if ui.checkbox(&mut selected, id).changed() {
                                toggled = Some(id.clone());
                            }
                        }
                    }
                }
            });
        if let Some(id) = toggled {
            editor.selection_toggle(&id);
        }
        ui.add_enabled_ui(!editor.fetch_selected.is_empty(), |ui| {
            if primary_button(
                ui,
                format!("Apply selected ({})", editor.fetch_selected.len()),
            )
            .clicked()
            {
                editor.apply_fetched_selection();
            }
        });
    }
    refresh
}
