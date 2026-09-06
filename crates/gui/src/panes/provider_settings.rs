use crate::model::provider_settings::ProviderSettingsModel;
use crate::theme::text::h3;
use crate::theme::tokens::{ERROR_FG, INPUT, OVERLAY, SP_2, SP_4, SURFACE_RAISED};
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
    egui::Modal::new(egui::Id::new("provider-settings"))
        .backdrop_color(OVERLAY)
        .frame(surface_frame(SURFACE_RAISED))
        .show(ctx, |ui| {
            ui.spacing_mut().item_spacing = egui::vec2(SP_2, SP_2);
            ui.label(h3("Provider settings"));
            egui::Grid::new("provider-settings-grid")
                .spacing(egui::vec2(SP_4, SP_2))
                .show(ui, |ui| {
                    let name = ui.label("Name");
                    ui.add(egui::TextEdit::singleline(&mut model.name).background_color(INPUT))
                        .labelled_by(name.id);
                    ui.end_row();
                    let base_url = ui.label("Base URL");
                    ui.add(egui::TextEdit::singleline(&mut model.base_url).background_color(INPUT))
                        .labelled_by(base_url.id);
                    ui.end_row();
                    let api_key_env = ui.label("API key env var");
                    ui.add(
                        egui::TextEdit::singleline(&mut model.api_key_env)
                            .hint_text("Environment variable NAME, e.g. OPENAI_API_KEY")
                            .background_color(INPUT),
                    )
                    .labelled_by(api_key_env.id);
                    ui.end_row();
                    let models = ui.label("Models");
                    ui.add(
                        egui::TextEdit::multiline(&mut model.models_text)
                            .hint_text("one per line")
                            .desired_rows(3)
                            .background_color(INPUT),
                    )
                    .labelled_by(models.id);
                    ui.end_row();
                    let default_model = ui.label("Default model");
                    let models = model.parsed_models();
                    egui::ComboBox::from_id_salt("provider-default-model")
                        .selected_text(&model.default_model)
                        .show_ui(ui, |ui| {
                            for candidate in models {
                                ui.selectable_value(
                                    &mut model.default_model,
                                    candidate.clone(),
                                    candidate,
                                );
                            }
                        })
                        .response
                        .labelled_by(default_model.id);
                    ui.end_row();
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
