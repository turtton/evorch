use crate::theme::tokens::*;

/// Shown when the image generator has no chain: image generation is off (ADR 0038).
pub const IMAGE_GENERATION_DISABLED: &str = "Disabled (generate_image unavailable)";
/// What conversation and compactor chains accept.
pub(crate) const CHAT_ROUTES: &str =
    "Only routes of chat models are offered for agent roles and the compactor.";
/// What the image generator chain accepts.
pub(crate) const IMAGE_ROUTES: &str =
    "Only routes of image generation models on Codex or OpenAI profiles are offered.";

/// 推論強度の ComboBox。一覧外の現在値は `(custom)` として保持する。
pub(crate) fn effort_picker(
    ui: &mut egui::Ui,
    id_salt: impl std::hash::Hash + std::fmt::Debug,
    unset: &str,
    value: &mut Option<String>,
    choices: &[String],
) -> egui::Response {
    let custom = value
        .as_ref()
        .filter(|current| !choices.contains(current))
        .cloned();
    let selected = match (value.as_deref(), &custom) {
        (None, _) => unset.to_owned(),
        (Some(current), Some(_)) => format!("{current} (custom)"),
        (Some(current), None) => current.to_owned(),
    };
    egui::ComboBox::from_id_salt(id_salt)
        .selected_text(selected)
        .show_ui(ui, |ui| {
            ui.selectable_value(value, None, unset);
            for effort in choices {
                ui.selectable_value(value, Some(effort.clone()), effort);
            }
            if let Some(current) = custom {
                ui.selectable_value(value, Some(current.clone()), format!("{current} (custom)"));
            }
        })
        .response
}

pub(crate) fn optional_text(ui: &mut egui::Ui, label: &str, value: &mut Option<String>) {
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

pub(crate) fn generation(ui: &mut egui::Ui, value: &mut config::GenerationOverridesConfig) {
    optional_number(ui, "Temperature", (&mut value.temperature, 0.0..=2.0));
    optional_number(ui, "Max tokens", (&mut value.max_tokens, 1..=u32::MAX));
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
