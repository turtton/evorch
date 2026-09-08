use super::{CredentialMode, ModelsFetchState, ProviderSettingsModel};
use std::sync::{
    Arc,
    mpsc::{TryRecvError, channel},
};

impl ProviderSettingsModel {
    pub fn start_models_fetch(&mut self) {
        let api_key = std::env::var(&self.api_key_env)
            .ok()
            .filter(|key| !key.is_empty());
        self.start_models_fetch_with_key(api_key);
    }

    pub fn start_models_fetch_with_key(&mut self, api_key: Option<String>) {
        self.start_models_fetch_resolving(move || {
            api_key.ok_or_else(|| "API key env var is not set".into())
        });
    }

    pub fn start_models_fetch_with_store(
        &mut self,
        store: Option<Arc<dyn sandbox::CredentialStore>>,
    ) {
        match self.credential_mode {
            CredentialMode::Env => self.start_models_fetch(),
            CredentialMode::Keyring => {
                let account = self.name.clone();
                let input = sandbox::Secret::from(self.api_key_input.clone());
                self.start_models_fetch_resolving(move || {
                    if !input.expose().is_empty() {
                        return Ok(input.expose().to_owned());
                    }
                    let store = store.ok_or_else(|| {
                        "Credential store unavailable; use environment-variable mode".to_owned()
                    })?;
                    store
                        .get(&account)
                        .map_err(|_| "Could not read credential store".to_owned())?
                        .map(|secret| secret.expose().to_owned())
                        .ok_or_else(|| {
                            "No stored API key; enter a key or use environment-variable mode"
                                .to_owned()
                        })
                });
            }
        }
    }

    fn start_models_fetch_resolving(
        &mut self,
        resolve: impl FnOnce() -> Result<String, String> + Send + 'static,
    ) {
        self.models_fetch_state = ModelsFetchState::Loading;
        self.available_models = None;
        let base_url = self.base_url.clone();
        self.models_fetch_base_url = Some(base_url.clone());
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let result = resolve().and_then(|api_key| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| error.to_string())?;
                runtime
                    .block_on(providers::list_models(
                        &base_url,
                        &providers::ProviderAuth::new(api_key),
                    ))
                    .map_err(|error| error.to_string())
            });
            let _ = tx.send(result);
        });
        self.models_rx = Some(rx);
    }

    pub fn poll_models(&mut self) -> bool {
        let Some(rx) = self.models_rx.take() else {
            return false;
        };
        match rx.try_recv() {
            Ok(_) if Some(self.base_url.as_str()) != self.models_fetch_base_url.as_deref() => {
                self.available_models = None;
                self.models_fetch_state = ModelsFetchState::Failed(
                    "Base URL changed during fetch; result discarded".into(),
                );
                self.models_fetch_base_url = None;
                true
            }
            Ok(Ok(models)) => {
                self.models_fetch_base_url = None;
                self.available_models = Some(models);
                self.models_fetch_state = ModelsFetchState::Loaded;
                true
            }
            Ok(Err(error)) => {
                self.models_fetch_base_url = None;
                self.available_models = None;
                self.models_fetch_state = ModelsFetchState::Failed(error);
                true
            }
            Err(TryRecvError::Empty) => {
                self.models_rx = Some(rx);
                false
            }
            Err(TryRecvError::Disconnected) => {
                self.models_fetch_base_url = None;
                self.available_models = None;
                self.models_fetch_state =
                    ModelsFetchState::Failed("Model fetch finished without result".into());
                true
            }
        }
    }
}
