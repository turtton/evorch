use super::{TelemetryOverlay, TelemetryRow, TokenUsage, pricing::ModelKey};
use crate::model::{model_metadata::MetadataSources, provider_settings::ProviderSettingsModel};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RequestContext {
    pub key: ModelKey,
    pub usage: TokenUsage,
}

impl TelemetryRow {
    pub fn activity_label(&self) -> String {
        use event_bus::RunActivity;
        match self.activity {
            Some(RunActivity::Model) => "model".into(),
            Some(RunActivity::Tools) => self.current_tool.clone().unwrap_or_else(|| "tools".into()),
            Some(RunActivity::Children) => "agents".into(),
            Some(RunActivity::User) => "user input".into(),
            Some(RunActivity::Compaction) => "compaction".into(),
            Some(RunActivity::Idle) | None => {
                self.current_tool.clone().unwrap_or_else(|| "idle".into())
            }
        }
    }
    pub fn diagnostics_label(&self) -> String {
        let mut text = format!(
            "Cumulative input / output: {} / {}",
            self.usage.input, self.usage.output
        );
        if let Some(c) = &self.context_composition {
            text.push_str(&format!("\nLatest context estimate: {} / {}\nInstructions: {} · tool definitions: {}\nConversation: {} · tool outputs: {}\nEstimation: serialized UTF-8 bytes / 4",c.projected_tokens,c.window_tokens,c.instructions,c.tool_definitions,c.conversation,c.tool_outputs));
        }
        if let Some(at) = self
            .last_checkpoint
            .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
        {
            text.push_str(&format!("\nLast saved checkpoint: Unix {}", at.as_secs()));
        }
        if let Some(reason) = &self.checkpoint_failure {
            text.push_str(&format!("\nCheckpoint save failed: {reason}"));
        }
        text
    }

    pub fn context_pressure(&self) -> Option<u128> {
        let usage = self.latest_context.as_ref()?.usage;
        let window = u128::from(self.context_window.filter(|window| *window > 0)?);
        let output = if self.in_flight {
            self.output_tokens
        } else {
            usage.output
        };
        let used = u128::from(usage.input) + u128::from(output);
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
