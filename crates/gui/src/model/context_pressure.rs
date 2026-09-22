use super::{TelemetryOverlay, TelemetryRow, TokenUsage, pricing::ModelKey};
use crate::model::{model_metadata::MetadataSources, provider_settings::ProviderSettingsModel};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RequestContext {
    pub key: ModelKey,
    pub usage: TokenUsage,
}

impl TelemetryRow {
    pub fn context_pressure(&self) -> Option<u128> {
        let usage = self.latest_context.as_ref()?.usage;
        let window = u128::from(self.context_window.filter(|window| *window > 0)?);
        let used =
            u128::from(usage.input) + u128::from(usage.cache_read) + u128::from(usage.cache_write);
        Some((used * 100 + window / 2) / window)
    }

    pub fn context_pressure_label(&self) -> Option<String> {
        self.context_pressure().map(|percent| format!("{percent}%"))
    }

    pub fn tokens_label(&self) -> String {
        let tokens = format!("{} / {}", self.usage.input, self.usage.output);
        match self.context_pressure_label() {
            Some(pressure) => format!("{tokens} ({pressure})"),
            None => tokens,
        }
    }
}

impl TelemetryOverlay {
    pub(super) fn refresh_context_windows(&mut self, settings: &ProviderSettingsModel) {
        let sources = MetadataSources {
            presets: &settings.model_presets,
            catalog: settings.catalog.catalog.as_deref(),
        };
        for row in self.rows.values_mut() {
            row.context_window = row.latest_context.as_ref().and_then(|request| {
                let key = &request.key;
                let fallback = config::ModelEntryConfig::enabled(&key.model);
                let entry = settings
                    .model_entry(key.profile.as_deref().unwrap_or(&key.provider), &key.model)
                    .unwrap_or(&fallback);
                let provider_type = settings.provider_type(key.profile.as_deref());
                sources
                    .resolve(
                        entry,
                        key.profile.as_deref().unwrap_or(&key.provider),
                        provider_type,
                    )
                    .context_window
            });
        }
    }
}
