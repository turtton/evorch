use super::WorkbenchState;
use crate::model::composer::ProviderStatus;
use crate::model::tasks::AgentRunSource;

impl<S: AgentRunSource> WorkbenchState<S> {
    pub fn open_provider_settings(&mut self) {
        self.provider_settings.error = None;
        self.provider_settings.open = true;
    }

    pub const fn close_provider_settings(&mut self) {
        self.provider_settings.open = false;
    }

    pub fn submit_provider_settings(&mut self) {
        let input = self.provider_settings.to_input();
        match self.provider_settings_path.as_deref() {
            None => {
                self.provider_settings.error = Some("No project config path is configured".into());
            }
            Some(path) => match config::save_openai_compatible_provider(path, &input) {
                Ok(()) => {
                    let notice = format!("Provider '{}' saved to {}", input.name, path.display());
                    self.provider_status = ProviderStatus::Configured;
                    self.close_provider_settings();
                    self.push_notice(notice);
                }
                Err(error) => self.provider_settings.error = Some(error.to_string()),
            },
        }
    }
}
