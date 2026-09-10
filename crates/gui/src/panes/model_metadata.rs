use config::{MetadataSource, ModelEntryConfig};

use crate::model::model_catalog::{CatalogRequest, CatalogState};
use crate::model::model_metadata::MetadataSources;
use crate::theme::{text::muted, tokens::ERROR_FG};

pub fn catalog_toolbar(ui: &mut egui::Ui, state: &mut CatalogState) {
    state.poll();
    ui.horizontal_wrapped(|ui| {
        if ui
            .add_enabled(!state.is_busy(), egui::Button::new("Model catalog Refresh"))
            .clicked()
        {
            state.start(CatalogRequest::ForceRefresh);
        }
        if state.is_busy() {
            ui.spinner();
        }
        let updated = state
            .modified
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok());
        ui.label(muted(updated.map_or_else(
            || "Last refresh: not loaded".into(),
            |time| format!("Last refresh: {} (Unix seconds)", time.as_secs()),
        )));
    });
    if let Some(error) = &state.error {
        ui.colored_label(ERROR_FG, error);
    }
    if state.is_busy() {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(200));
    }
}

pub fn model_metadata(
    ui: &mut egui::Ui,
    entry: &mut ModelEntryConfig,
    sources: &MetadataSources<'_>,
) {
    let label = ui.label("Preset");
    egui::ComboBox::from_id_salt("metadata-preset")
        .selected_text(entry.preset.as_deref().unwrap_or("(none)"))
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut entry.preset, None, "(none)");
            for name in sources.presets.keys() {
                ui.selectable_value(&mut entry.preset, Some(name.clone()), name);
            }
        })
        .response
        .labelled_by(label.id);
    let label = ui.label("Metadata source");
    let selected = match entry.metadata_source {
        Some(MetadataSource::Manual) => "manual",
        Some(MetadataSource::ModelsDev) => "models-dev",
        Some(MetadataSource::ProviderDefault) | None => "provider-default",
    };
    egui::ComboBox::from_id_salt("metadata-source")
        .selected_text(selected)
        .show_ui(ui, |ui| {
            for (value, label) in [
                (MetadataSource::Manual, "manual"),
                (MetadataSource::ModelsDev, "models-dev"),
                (MetadataSource::ProviderDefault, "provider-default"),
            ] {
                ui.selectable_value(&mut entry.metadata_source, Some(value), label);
            }
        })
        .response
        .labelled_by(label.id);
    let label = ui.label("models.dev reference");
    let mut reference = entry.metadata_ref.clone().unwrap_or_default();
    if ui
        .add(egui::TextEdit::singleline(&mut reference).hint_text("provider/model or model ID"))
        .labelled_by(label.id)
        .changed()
    {
        entry.metadata_ref = (!reference.trim().is_empty()).then(|| reference.trim().to_owned());
    }
    let label = ui.label("Context window override");
    let input_id = ui.id().with("context-override");
    let mut draft = ui
        .data_mut(|data| data.get_temp::<String>(input_id))
        .unwrap_or_else(|| {
            entry
                .context_window
                .map_or_else(String::new, |value| value.to_string())
        });
    let response = ui
        .add(egui::TextEdit::singleline(&mut draft).hint_text("Empty: use preset / catalog"))
        .labelled_by(label.id);
    let parsed = if draft.trim().is_empty() {
        Ok(None)
    } else {
        draft.trim().parse::<u64>().map(Some)
    };
    match parsed {
        Ok(value) => {
            if response.changed() {
                entry.context_window = value;
            }
        }
        Err(_) => {
            ui.colored_label(
                ERROR_FG,
                "Enter a non-negative integer; previous override is retained",
            );
        }
    }
    ui.data_mut(|data| data.insert_temp(input_id, draft));
}
