use crate::model::provider_settings::{ModelsFetchState, ProviderSettingsModel};
use crate::theme::text::{h3, muted};
use crate::theme::tokens::{
    ERROR_FG, FONT_SMALL, INPUT, OVERLAY, PROVIDER_MODAL_MAX_WIDTH, SP_2, SP_3, SP_4,
    SURFACE_RAISED,
};
use crate::theme::widgets::{primary_button, surface_frame};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderSettingsAction {
    Save,
    Cancel,
}

pub fn provider_settings_modal(
    ctx: &egui::Context,
    model: &mut ProviderSettingsModel,
) -> Option<ProviderSettingsAction> {
    let mut action = None;
    let viewport_width = ctx.viewport_rect().width();
    let modal_width = (viewport_width * 0.6).min(PROVIDER_MODAL_MAX_WIDTH);
    let content_width = modal_width - 2.0 * (SP_3 + 1.0);
    let input_width = (content_width - 146.0).max(200.0);
    egui::Modal::new(egui::Id::new("provider-settings"))
        .backdrop_color(OVERLAY)
        .frame(surface_frame(SURFACE_RAISED))
        .show(ctx, |ui| {
            ui.set_max_width(content_width);
            ui.spacing_mut().item_spacing = egui::vec2(SP_2, SP_2);
            ui.label(h3("Provider settings"));
            egui::Grid::new("provider-settings-grid")
                .spacing(egui::vec2(SP_4, SP_2))
                .show(ui, |ui| {
                    let name = ui.label("Name");
                    ui.add(
                        egui::TextEdit::singleline(&mut model.name)
                            .desired_width(input_width)
                            .background_color(INPUT),
                    )
                    .labelled_by(name.id);
                    ui.end_row();
                    let base_url = ui.label("Base URL");
                    ui.add(
                        egui::TextEdit::singleline(&mut model.base_url)
                            .desired_width(input_width)
                            .background_color(INPUT),
                    )
                    .labelled_by(base_url.id);
                    ui.end_row();
                    let api_key_env = ui.label("API key env var");
                    ui.add(
                        egui::TextEdit::singleline(&mut model.api_key_env)
                            .hint_text("Environment variable NAME, e.g. OPENAI_API_KEY")
                            .desired_width(input_width)
                            .background_color(INPUT),
                    )
                    .labelled_by(api_key_env.id);
                    ui.end_row();
                    let models = ui.label("Models");
                    ui.add(
                        egui::TextEdit::multiline(&mut model.models_text)
                            .hint_text("one per line")
                            .desired_rows(3)
                            .desired_width(input_width)
                            .background_color(INPUT),
                    )
                    .labelled_by(models.id);
                    ui.end_row();
                    let excluded_models = ui.label("Excluded models");
                    ui.add(
                        egui::TextEdit::multiline(&mut model.excluded_models_text)
                            .hint_text("one per line")
                            .desired_rows(3)
                            .desired_width(input_width)
                            .background_color(INPUT),
                    )
                    .labelled_by(excluded_models.id);
                    ui.end_row();
                    let default_model = ui.label("Default model");
                    let choices = model.candidate_models();
                    let max_display_chars =
                        ((input_width - 24.0) / 8.0).max(10.0) as usize;
                    let default_model_text =
                        if model.default_model.chars().count() > max_display_chars {
                            format!(
                                "{}…",
                                model
                                    .default_model
                                    .chars()
                                    .take(max_display_chars)
                                    .collect::<String>()
                            )
                        } else {
                            model.default_model.clone()
                        };
                    let combo = egui::ComboBox::from_id_salt("provider-default-model")
                        .width(input_width)
                        .selected_text(default_model_text)
                        .show_ui(ui, |ui| {
                            for candidate in choices {
                                ui.selectable_value(
                                    &mut model.default_model,
                                    candidate.clone(),
                                    candidate,
                                );
                            }
                        });
                    combo.response.labelled_by(default_model.id);
                    ui.end_row();
                });
            ui.add(egui::Label::new(muted(
                "Used when a route doesn't override the model and when re-resolving a pinned session.",
            )).wrap());
            ui.horizontal(|ui| {
                if ui
                    .add(egui::Button::new("Refresh models").small())
                    .clicked()
                {
                    model.start_models_fetch();
                }
                match &model.models_fetch_state {
                    ModelsFetchState::Idle => {}
                    ModelsFetchState::Loading => {
                        ui.label(muted("Loading models…"));
                    }
                    ModelsFetchState::Loaded => {
                        let count = model
                            .available_models
                            .as_ref()
                            .map_or(0, |models| models.len());
                        ui.label(muted(format!(
                            "Loaded {count} models from /v1/models"
                        )));
                    }
                    ModelsFetchState::Failed(error) => {
                        ui.label(
                            egui::RichText::new(format!(
                                "Auto-fetch failed ({error}); manual entry below"
                            ))
                            .color(ERROR_FG)
                            .size(FONT_SMALL),
                        );
                    }
                }
            });
            if let Some(error) = &model.error {
                ui.label(egui::RichText::new(error).color(ERROR_FG));
            }
            ui.horizontal(|ui| {
                if primary_button(ui, "Save").clicked() {
                    action = Some(ProviderSettingsAction::Save);
                }
                if ui.add(egui::Button::new("Cancel")).clicked() {
                    action = Some(ProviderSettingsAction::Cancel);
                }
            });
        });
    action
}
