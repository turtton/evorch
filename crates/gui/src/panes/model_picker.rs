use runtime::compose::ProfileSummary;
use workspace_ui::ModelPreference;

use crate::model::model_picker::{ModelPickerState, preference_label, profile_options};

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
        .map(preference_label)
        .or_else(|| context.default_model.map(str::to_owned))
        .unwrap_or_else(|| "Select model".into());
    let response = ui
        .add_enabled_ui(context.enabled && !context.profiles.is_empty(), |ui| {
            egui::ComboBox::from_id_salt("model-picker")
                .selected_text(&label)
                .width(ui.available_width().min(200.0))
                .truncate()
                .show_ui(ui, |ui| {
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
                                    context.preference == Some(&preference),
                                    egui::RichText::new(preference_label(&preference))
                                        .color(crate::theme::tokens::palette().TEXT),
                                )
                                .clicked()
                            {
                                selected = Some(Some(preference.clone()));
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
