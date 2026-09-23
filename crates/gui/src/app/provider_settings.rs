use super::WorkbenchState;
use crate::model::provider_settings::{ProfileEditor, ProviderSettingsModel};
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
        if self.settings_save_in_progress() {
            return;
        }
        self.routing_settings.open = false;
        self.sandbox_settings.open = false;
        self.close_theme_settings();
        self.role_settings.open = false;
        self.provider_settings.error = None;
        self.provider_settings.editor = None;
        self.provider_settings.open = true;
    }

    pub fn close_provider_settings(&mut self) {
        if self.provider_settings.editor.take().is_none() {
            self.provider_settings.open = false;
        }
        self.provider_settings.error = None;
    }

    pub fn start_codex_login(&mut self) {
        self.prepare_codex_editor();
        if let Some(editor) = self.provider_settings.codex_mut() {
            editor.auth.start();
        }
    }

    pub fn prepare_codex_editor(&mut self) {
        let store = self.credential_store.clone();
        let Some(editor) = self.provider_settings.codex_mut() else {
            return;
        };
        // Load the provider's current limits when editing an existing profile.
        // A failed fetch stays visible until the user explicitly retries.
        if editor.original_name.is_some()
            && matches!(
                &editor.fetch.models_fetch_state,
                crate::model::provider_settings::ModelsFetchState::Idle
            )
            && store.is_some()
        {
            editor.start_models_fetch_with_store(store.clone());
        }
        if editor.auth.has_backend() && editor.auth.credential_account == editor.account {
            return;
        }
        if let Some(store) = store {
            match crate::model::codex_auth_backend::ProviderCodexAuthBackend::production(
                store,
                editor.account.clone(),
                "https://auth.openai.com",
            ) {
                Ok(backend) => {
                    editor.auth = crate::model::codex_auth::CodexAuthModel::with_backend(
                        std::sync::Arc::new(backend),
                        editor.account.clone(),
                    )
                }
                Err(failure) => {
                    editor.auth.state = crate::model::codex_auth::CodexAuthState::Failed { failure }
                }
            }
        } else if self.codex_auth.has_backend()
            && self.codex_auth.credential_account == editor.account
        {
            std::mem::swap(&mut editor.auth, &mut self.codex_auth);
        }
    }

    pub fn submit_provider_settings(&mut self) {
        if self.settings_save_in_progress() {
            return;
        }
        let Some(path) = self.provider_settings_path.clone() else {
            self.provider_settings.error = Some("No project config path is configured".into());
            return;
        };
        let store = self.credential_store.clone();
        match &self.provider_settings.editor {
            None => {}
            Some(ProfileEditor::OpenAiCompatible(editor)) => {
                let input = editor.to_input();
                if let Err(error) = config::validate_openai_compatible_provider_input(&input) {
                    self.provider_settings.error = Some(error.to_string());
                    return;
                }
                let original_name = editor.original_name.clone();
                let original_credential = editor.original_credential.clone();
                let secret = sandbox::Secret::from(editor.api_key_input.clone());
                self.provider_operation(move || {
                    let mut migrated_account = None;
                    match &input.credential {
                        config::ProviderCredentialInput::Env { .. } => {}
                        config::ProviderCredentialInput::Keyring { account, .. } => {
                            let store = store.as_ref().ok_or_else(|| {
                                "Credential store unavailable; use environment-variable mode"
                                    .to_owned()
                            })?;
                            let original_account = match &original_credential {
                                Some(config::CredentialRefConfig::Keyring { account, .. }) => {
                                    Some(account)
                                }
                                Some(config::CredentialRefConfig::Env { .. }) | None => None,
                            };
                            let secret = if secret.expose().is_empty() {
                                let source = original_account
                                    .filter(|_| original_name.is_some())
                                    .ok_or_else(|| {
                                    "Enter an API key before saving".to_owned()
                                })?;
                                store
                                    .get(source)
                                    .map_err(|e| e.to_string())?
                                    .filter(|value| !value.expose().trim().is_empty())
                                    .ok_or_else(|| "Enter an API key before saving".to_owned())?
                            } else {
                                secret
                            };
                            store.set(account, &secret).map_err(|e| e.to_string())?;
                            migrated_account =
                                original_account.filter(|original| *original != account);
                        }
                    }
                    config::save::save_openai_compatible_provider_edit(
                        &path,
                        &input,
                        original_name.as_deref(),
                    )
                    .map_err(|e| e.to_string())?;
                    if let (Some(store), Some(account)) = (store, migrated_account) {
                        store.delete(account).map_err(|e| e.to_string())?;
                    }
                    Ok(())
                });
            }
            Some(ProfileEditor::Codex(editor)) => {
                let original_name = editor.original_name.clone();
                let input = config::CodexProviderInput {
                    name: editor.name.clone(),
                    account: editor.account.clone(),
                    base_url: editor.fetch.base_url.clone(),
                    models: editor
                        .models
                        .iter()
                        .map(|id| {
                            editor
                                .model_entries
                                .get(id)
                                .cloned()
                                .unwrap_or_else(|| config::ModelEntryConfig::enabled(id))
                        })
                        .collect(),
                    default_model: editor.default_model.clone(),
                };
                self.provider_operation(move || {
                    config::save_codex_provider_edit(&path, &input, original_name.as_deref())
                        .map_err(|e| e.to_string())
                });
            }
        }
    }

    pub fn delete_provider_settings(&mut self, name: String) {
        if self.settings_save_in_progress() {
            return;
        }
        let Some(path) = self.provider_settings_path.clone() else {
            self.provider_settings.error = Some("No project config path is configured".into());
            return;
        };
        let credential = self.provider_settings.credential(&name).cloned();
        let store = self.credential_store.clone();
        self.provider_operation(move || {
            config::delete_provider(&path, &name).map_err(|e| e.to_string())?;
            if let Some(config::CredentialRefConfig::Keyring { account, .. }) = credential {
                if let Some(store) = store {
                    if let Err(error) = store.delete(&account) {
                        tracing::warn!(%error, "provider credential deletion failed");
                    }
                } else {
                    tracing::warn!("provider credential deletion skipped: store unavailable");
                }
            }
            Ok(())
        });
    }

    fn provider_operation(
        &mut self,
        operation: impl FnOnce() -> Result<(), String> + Send + 'static,
    ) {
        let production = self.production_model.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        self.provider_save_rx = Some(rx);
        std::thread::spawn(move || {
            let result = operation().and_then(|()| {
                if let Some((context, model)) = production {
                    model.replace(context.reload()?);
                }
                Ok(())
            });
            if let Err(error) = &result {
                tracing::error!(%error, "provider update or recomposition failed");
            }
            let _ = tx.send(result);
        });
    }

    pub fn poll_provider_save(&mut self) {
        self.receive_provider_save(std::time::Duration::ZERO);
    }

    pub(crate) fn receive_provider_save(&mut self, timeout: std::time::Duration) {
        let Some(rx) = self.provider_save_rx.take() else {
            return;
        };
        match rx.recv_timeout(timeout) {
            Ok(result) => {
                if let Err(error) = result {
                    self.provider_settings.error = Some(error);
                    return;
                }
                if let Some(path) = &self.provider_settings_path {
                    let options = config::LoadOptions {
                        project_dir: path.parent().map(std::path::Path::to_path_buf),
                        read_env: false,
                        ..Default::default()
                    };
                    match config::Config::load(&options) {
                        Ok(config) => {
                            self.provider_status =
                                crate::model::provider_settings::provider_status_of(&config);
                            let catalog = std::mem::take(&mut self.provider_settings.catalog);
                            self.provider_settings =
                                ProviderSettingsModel::seed_from_config(&config);
                            self.provider_settings.catalog = catalog;
                            self.provider_settings.open = true;
                        }
                        Err(error) => {
                            self.provider_settings.error = Some(error.to_string());
                            return;
                        }
                    }
                }
                self.push_notice("Provider settings updated");
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => self.provider_save_rx = Some(rx),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                self.provider_settings.error =
                    Some("Credential worker stopped without a result".into())
            }
        }
    }
}
