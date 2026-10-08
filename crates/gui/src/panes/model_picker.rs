use runtime::compose::ProfileSummary;
use workspace_ui::ModelPreference;

use crate::model::model_picker::{
    ModelPickerState, effort_choices, preference_label, profile_options, same_model, switch_model,
};

#[derive(Clone, Copy)]
pub struct ModelPickerContext<'a> {
    pub profiles: &'a [ProfileSummary],
    pub preference: Option<&'a ModelPreference>,
    /// The role's routed model, shown while no explicit preference is set.
    pub default_model: Option<&'a str>,
    pub enabled: bool,
}

pub fn model_picker(
    ui: &mut egui::Ui,
    context: ModelPickerContext<'_>,
    state: &mut ModelPickerState,
) -> Option<Option<ModelPreference>> {
    let mut selected = None;
    let label = context
        .preference
        .map(|preference| match &preference.reasoning_effort {
            Some(effort) => format!("{} · {effort}", preference_label(preference)),
            None => preference_label(preference),
        })
        .or_else(|| context.default_model.map(str::to_owned))
        .unwrap_or_else(|| "Select model".into());
    let response = ui
        .add_enabled_ui(context.enabled && !context.profiles.is_empty(), |ui| {
            egui::ComboBox::from_id_salt("model-picker")
                .selected_text(&label)
                .width(ui.available_width().min(200.0))
                .truncate()
                .show_ui(ui, |ui| {
                    // Effort belongs to an explicit selection; automatic routing uses
                    // each candidate's configured effort.
                    if let Some(preference) = context.preference
                        && let efforts = effort_choices(preference, context.profiles)
                        && !efforts.is_empty()
                    {
                        ui.label(crate::theme::text::muted("Reasoning effort"));
                        let current = preference.reasoning_effort.as_ref();
                        let mut options = vec![None];
                        options.extend(efforts.into_iter().map(Some));
                        if let Some(custom) =
                            current.filter(|effort| !options.contains(&Some((*effort).clone())))
                        {
                            options.push(Some(custom.clone()));
                        }
                        for effort in options {
                            let text = effort.clone().unwrap_or_else(|| "Default effort".into());
                            if ui
                                .selectable_label(current == effort.as_ref(), text)
                                .clicked()
                            {
                                selected = Some(Some(ModelPreference {
                                    reasoning_effort: effort,
                                    ..preference.clone()
                                }));
                            }
                        }
                        ui.separator();
                    }
                    if ui
                        .selectable_label(context.preference.is_none(), "Automatic routing")
                        .clicked()
                    {
                        selected = Some(None);
                    }
                    for preference in profile_options(context.profiles) {
                        ui.push_id((&preference.profile, &preference.model), |ui| {
                            if ui
                                .selectable_label(
                                    context
                                        .preference
                                        .is_some_and(|current| same_model(current, &preference)),
                                    egui::RichText::new(preference_label(&preference))
                                        .color(crate::theme::tokens::palette().TEXT),
                                )
                                .clicked()
                            {
                                selected = Some(Some(switch_model(
                                    context.preference,
                                    preference.clone(),
                                    context.profiles,
                                )));
                            }
                        });
                    }
                })
        })
        .inner;
    response.response.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::ComboBox,
            context.enabled && !context.profiles.is_empty(),
            &label,
        )
    });
    state.open = response.inner.is_some();
    state.hovered = response.response.hovered();
    selected
}
