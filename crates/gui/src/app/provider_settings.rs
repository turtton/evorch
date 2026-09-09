use super::WorkbenchState;
use crate::model::composer::ProviderStatus;
use crate::model::tasks::AgentRunSource;

impl<S: AgentRunSource> WorkbenchState<S> {
    pub fn with_production_model(
        mut self,
        context: crate::model::production::ProductionModel,
        model: std::sync::Arc<runtime::compose::SwitchableModel>,
    ) -> Self {
        self.production_model = Some((context, model));
        self
    }

    pub fn open_provider_settings(&mut self) {
        self.codex_auth.refresh_from_store();
        self.provider_settings.error = None;
        self.provider_settings.open = true;
        self.provider_settings
            .start_models_fetch_with_store(self.credential_store.clone());
    }

    pub const fn close_provider_settings(&mut self) {
        self.provider_settings.open = false;
    }

    pub fn start_codex_login(&mut self) {
        self.codex_auth.start();
    }

    pub fn submit_provider_settings(&mut self) {
        if self.provider_save_rx.is_some()
            || self.provider_settings.tab
                == crate::model::provider_settings::ProviderSettingsTab::Codex
        {
            return;
        }
        let input = self.provider_settings.to_input();
        if let Err(error) = config::validate_openai_compatible_provider_input(&input) {
            self.provider_settings.error = Some(error.to_string());
            return;
        }
        if let Some((context, model)) = self.production_model.clone() {
            let Some(path) = self.provider_settings_path.clone() else {
                self.provider_settings.error = Some("No project config path is configured".into());
                return;
            };
            let secret = sandbox::Secret::from(self.provider_settings.api_key_input.clone());
            let mode = self.provider_settings.credential_mode;
            let (tx, rx) = std::sync::mpsc::channel();
            self.provider_save_rx = Some(rx);
            std::thread::spawn(move || {
                let result = (|| {
                    match mode {
                        crate::model::provider_settings::CredentialMode::Env => {}
                        crate::model::provider_settings::CredentialMode::Keyring => {
                            if secret.expose().is_empty() {
                                let existing = context
                                    .credential_store
                                    .get(&input.name)
                                    .map_err(|error| error.to_string())?;
                                if existing.is_none_or(|value| value.expose().trim().is_empty()) {
                                    return Err("Enter an API key before saving".to_owned());
                                }
                            } else {
                                context
                                    .credential_store
                                    .set(&input.name, &secret)
                                    .map_err(|error| error.to_string())?;
                            }
                        }
                    }
                    config::save_openai_compatible_provider(&path, &input)
                        .map_err(|error| error.to_string())?;
                    let replacement = context.reload()?;
                    model.replace(replacement);
                    Ok(())
                })();
                if let Err(error) = &result {
                    tracing::error!(%error, "provider save or recomposition failed");
                }
                let _ = tx.send(result);
            });
            return;
        }
        match self.provider_settings.credential_mode {
            crate::model::provider_settings::CredentialMode::Env => {}
            crate::model::provider_settings::CredentialMode::Keyring => {
                let Some(store) = self.credential_store.clone() else {
                    self.provider_settings.error =
                        Some("Credential store unavailable; use environment-variable mode".into());
                    return;
                };
                let Some(path) = self.provider_settings_path.clone() else {
                    self.provider_settings.error =
                        Some("No project config path is configured".into());
                    return;
                };
                let secret = sandbox::Secret::from(self.provider_settings.api_key_input.clone());
                let (tx, rx) = std::sync::mpsc::channel();
                self.provider_save_rx = Some(rx);
                std::thread::spawn(move || {
                    let result = if secret.expose().is_empty() {
                        store
                            .get(&input.name)
                            .map_err(|_| "Could not read credential store".to_owned())
                            .and_then(|value| {
                                value
                                    .map(|_| ())
                                    .ok_or_else(|| "Enter an API key before saving".into())
                            })
                    } else {
                        store
                            .set(&input.name, &secret)
                            .map_err(|_| "Could not save API key to credential store".to_owned())
                    }
                    .and_then(|()| {
                        config::save_openai_compatible_provider(&path, &input)
                            .map_err(|error| error.to_string())
                    });
                    let _ = tx.send(result);
                });
                return;
            }
        }
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

    pub fn poll_provider_save(&mut self) {
        let Some(rx) = self.provider_save_rx.take() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(())) => {
                self.provider_settings.api_key_input.clear();
                self.provider_settings.api_key_stored = self.provider_settings.credential_mode
                    == crate::model::provider_settings::CredentialMode::Keyring;
                self.provider_settings.error = None;
                self.provider_status = ProviderStatus::Configured;
                self.close_provider_settings();
                self.push_notice("Provider saved");
            }
            Ok(Err(error)) => self.provider_settings.error = Some(error),
            Err(std::sync::mpsc::TryRecvError::Empty) => self.provider_save_rx = Some(rx),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.provider_settings.error =
                    Some("Credential worker stopped without a result".into())
            }
        }
    }
}
