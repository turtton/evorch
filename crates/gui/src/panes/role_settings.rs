use crate::model::role_settings::{CATEGORIES, RoleSettingsModel, effort_options};
use crate::theme::{
    text::{badge, h3, muted},
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
    let effort_choices = model.effort_choices.clone();
    egui::Modal::new(egui::Id::new("role-settings"))
        .backdrop_color(palette().OVERLAY)
        .frame(surface_frame(palette().SURFACE_RAISED))
        .show(ctx, |ui| {
            ui.set_width(
                (ctx.viewport_rect().width() * 0.6).min(PROVIDER_MODAL_MAX_WIDTH) - SP_4 * 4.0,
            );
            ui.spacing_mut().item_spacing = egui::vec2(SP_2, SP_2);
            ui.label(h3("Agent role settings"));
            ui.label(muted(
                "Worker categories take precedence over the worker default; other roles use their role model.",
            ));
            ui.add_enabled_ui(!busy, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("role-bindings")
                    .max_height((ctx.viewport_rect().height() - TOPBAR * 4.0).max(TOPBAR))
                    .show(ui, |ui| {
                        let agents = &mut model.agents;
                        for (name, binding, categories) in [
                            ("Orchestrator", &mut agents.orchestrator, None),
                            ("Explorer", &mut agents.explorer, None),
                            (
                                "Worker",
                                &mut agents.worker.base,
                                Some(&mut agents.worker.categories),
                            ),
                            ("Reviewer", &mut agents.reviewer, None),
                            ("Librarian", &mut agents.roles.librarian, None),
                            ("Planner", &mut agents.roles.planner, None),
                            ("Oracle", &mut agents.roles.oracle, None),
                            ("Multimodal Looker", &mut agents.roles.multimodal_looker, None),
                        ] {
                            ui.push_id(name, |ui| {
                                ui.collapsing(badge(name), |ui| {
                                    model_picker(
                                        ui,
                                        &mut binding.logical_model,
                                        (&model.logical_models, "Role logical model"),
                                    );
                                    optional_text(ui, "Preset reference", &mut binding.preset);
                                    ui.collapsing(badge("Generation overrides"), |ui| {
                                        generation(
                                            ui,
                                            &mut binding.generation,
                                            effort_options(&effort_choices, binding.logical_model.as_deref()),
                                        )
                                    });
                                    if let Some(categories) = categories {
                                        ui.label(muted("Category overrides"));
                                        for category in CATEGORIES {
                                            ui.collapsing(badge(category), |ui| {
                                                let mut draft = categories
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
                                                generation(
                                                    ui,
                                                    &mut draft.generation,
                                                    effort_options(
                                                        &effort_choices,
                                                        draft.logical_model.as_deref(),
                                                    ),
                                                );
                                                if ui.button("Reset category").clicked() {
                                                    draft = Default::default();
                                                }
                                                if draft == config::CategoryBindingConfig::default() {
                                                    categories.remove(category);
                                                } else {
                                                    categories.insert(category.into(), draft);
                                                }
                                            });
                                        }
                                    }
                                });
                            });
                        }
                    });
            });
            if let Some(error) = &model.error {
                ui.colored_label(palette().ERROR_FG, error);
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
                .background_color(palette().INPUT)
                .hint_text("Inherit default"),
        )
        .labelled_by(label.id)
        .changed()
    {
        *value = (!text.trim().is_empty()).then(|| text.trim().into());
    }
}

fn generation(
    ui: &mut egui::Ui,
    value: &mut config::GenerationOverridesConfig,
    efforts: Vec<String>,
) {
    optional_number(ui, "Temperature", (&mut value.temperature, 0.0..=2.0));
    optional_number(ui, "Top p", (&mut value.top_p, 0.0..=1.0));
    optional_number(ui, "Max tokens", (&mut value.max_tokens, 1..=u32::MAX));
    let label = ui.label("Reasoning effort");
    let selected = value
        .reasoning_effort
        .clone()
        .map(|effort| {
            if efforts.iter().any(|level| level == &effort) {
                effort
            } else {
                format!("{effort} (custom)")
            }
        })
        .unwrap_or_else(|| "Inherit default".to_owned());
    let custom = value
        .reasoning_effort
        .as_ref()
        .filter(|current| !efforts.iter().any(|level| level == *current))
        .cloned();
    egui::ComboBox::from_id_salt("reasoning-effort")
        .selected_text(selected)
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut value.reasoning_effort, None, "Inherit default");
            for effort in &efforts {
                ui.selectable_value(
                    &mut value.reasoning_effort,
                    Some(effort.clone()),
                    effort,
                );
            }
            if let Some(current) = custom {
                ui.selectable_value(
                    &mut value.reasoning_effort,
                    Some(current.clone()),
                    format!("{current} (custom)"),
                );
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
