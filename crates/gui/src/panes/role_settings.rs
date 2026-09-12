use crate::model::role_settings::{CATEGORIES, RoleSettingsModel};
use crate::theme::{
    text::{h3, muted},
    tokens::*,
    widgets::{primary_button, surface_frame},
};

pub enum RoleSettingsAction {
    Save,
    Cancel,
}

pub fn role_settings_modal(
    ctx: &egui::Context,
    model: &mut RoleSettingsModel,
) -> Option<RoleSettingsAction> {
    let mut action = None;
    let busy = model.is_saving();
    egui::Modal::new(egui::Id::new("role-settings"))
        .backdrop_color(OVERLAY)
        .frame(surface_frame(SURFACE_RAISED))
        .show(ctx, |ui| {
            ui.set_width(
                (ctx.viewport_rect().width() * 0.6).min(PROVIDER_MODAL_MAX_WIDTH) - SP_4 * 4.0,
            );
            ui.spacing_mut().item_spacing = egui::vec2(SP_2, SP_2);
            ui.label(h3("Agent role settings"));
            ui.label(muted(
                "Category overrides take precedence; unset fields inherit the role default.",
            ));
            ui.add_enabled_ui(!busy, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("role-bindings")
                    .max_height((ctx.viewport_rect().height() - TOPBAR * 4.0).max(TOPBAR))
                    .show(ui, |ui| {
                        let agents = &mut model.agents;
                        for (name, binding) in [
                            ("Orchestrator", &mut agents.orchestrator),
                            ("Explorer", &mut agents.explorer),
                            ("Worker", &mut agents.worker),
                            ("Reviewer", &mut agents.reviewer),
                            ("Planner", &mut agents.roles.planner),
                            ("Oracle", &mut agents.roles.oracle),
                            ("Multimodal Looker", &mut agents.roles.multimodal_looker),
                        ] {
                            ui.push_id(name, |ui| {
                                ui.collapsing(name, |ui| {
                                    model_picker(
                                        ui,
                                        &mut binding.logical_model,
                                        (&model.logical_models, "Role logical model"),
                                    );
                                    optional_text(ui, "Preset reference", &mut binding.preset);
                                    ui.collapsing("Generation overrides", |ui| {
                                        generation(ui, &mut binding.generation)
                                    });
                                    ui.label(muted("Category overrides"));
                                    for category in CATEGORIES {
                                        ui.collapsing(category, |ui| {
                                            let mut draft = binding
                                                .categories
                                                .get(category)
                                                .cloned()
                                                .unwrap_or_default();
                                            model_picker(
                                                ui,
                                                &mut draft.logical_model,
                                                (
                                                    &model.logical_models,
                                                    &format!("{category} logical model"),
                                                ),
                                            );
                                            optional_text(
                                                ui,
                                                "Category preset reference",
                                                &mut draft.preset,
                                            );
                                            generation(ui, &mut draft.generation);
                                            if ui.button("Reset category").clicked() {
                                                draft = Default::default();
                                            }
                                            if draft == config::CategoryBindingConfig::default() {
                                                binding.categories.remove(category);
                                            } else {
                                                binding.categories.insert(category.into(), draft);
                                            }
                                        });
                                    }
                                });
                            });
                        }
                        ui.label(muted(
                            "Librarian: not available in the current config schema",
                        ));
                    });
            });
            if let Some(error) = &model.error {
                ui.colored_label(ERROR_FG, error);
            }
            if busy {
                ui.label(muted("Saving and reloading runtime..."));
            }
            ui.add_enabled_ui(!busy, |ui| {
                ui.horizontal(|ui| {
                    if primary_button(ui, "Save role settings").clicked() {
                        action = Some(RoleSettingsAction::Save);
                    }
                    if ui.button("Cancel").clicked() {
                        action = Some(RoleSettingsAction::Cancel);
                    }
                })
            });
        });
    action
}

fn model_picker(ui: &mut egui::Ui, value: &mut Option<String>, choices: (&[String], &str)) {
    let label = ui.label(choices.1);
    egui::ComboBox::from_id_salt(choices.1)
        .width(ui.available_width())
        .selected_text(value.as_deref().unwrap_or("Inherit default"))
        .show_ui(ui, |ui| {
            ui.selectable_value(value, None, "Inherit default");
            for name in choices.0 {
                ui.selectable_value(value, Some(name.clone()), name);
            }
        })
        .response
        .labelled_by(label.id);
}

fn optional_text(ui: &mut egui::Ui, label: &str, value: &mut Option<String>) {
    let label = ui.label(label);
    let mut text = value.clone().unwrap_or_default();
    if ui
        .add(
            egui::TextEdit::singleline(&mut text)
                .desired_width(ui.available_width())
                .background_color(INPUT)
                .hint_text("Inherit default"),
        )
        .labelled_by(label.id)
        .changed()
    {
        *value = (!text.trim().is_empty()).then(|| text.trim().into());
    }
}

fn generation(ui: &mut egui::Ui, value: &mut config::GenerationOverridesConfig) {
    optional_number(ui, "Temperature", (&mut value.temperature, 0.0..=2.0));
    optional_number(ui, "Top p", (&mut value.top_p, 0.0..=1.0));
    optional_number(ui, "Max tokens", (&mut value.max_tokens, 1..=u32::MAX));
    let label = ui.label("Reasoning effort");
    egui::ComboBox::from_id_salt("reasoning-effort")
        .selected_text(match value.reasoning_effort {
            None => "Inherit default",
            Some(config::ReasoningEffortConfig::Low) => "Low",
            Some(config::ReasoningEffortConfig::Medium) => "Medium",
            Some(config::ReasoningEffortConfig::High) => "High",
        })
        .show_ui(ui, |ui| {
            for (value_option, name) in [
                (None, "Inherit default"),
                (Some(config::ReasoningEffortConfig::Low), "Low"),
                (Some(config::ReasoningEffortConfig::Medium), "Medium"),
                (Some(config::ReasoningEffortConfig::High), "High"),
            ] {
                ui.selectable_value(&mut value.reasoning_effort, value_option, name);
            }
        })
        .response
        .labelled_by(label.id);
}

fn optional_number<T: egui::emath::Numeric>(
    ui: &mut egui::Ui,
    label: &str,
    field: (&mut Option<T>, std::ops::RangeInclusive<T>),
) {
    ui.horizontal(|ui| {
        let mut enabled = field.0.is_some();
        if ui.checkbox(&mut enabled, label).changed() {
            *field.0 = enabled.then(|| *field.1.start());
        }
        if let Some(value) = field.0 {
            ui.add(egui::DragValue::new(value).range(field.1));
        }
    });
}
