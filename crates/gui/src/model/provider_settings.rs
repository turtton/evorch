//! egui に依存しないプロバイダ設定の編集モデル。

use std::sync::mpsc::{Receiver, TryRecvError, channel};

use super::composer::{PROVIDER_MISSING_GUIDANCE, ProviderStatus};

/// /v1/models からのモデル一覧取得状態。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelsFetchState {
    Idle,
    Loading,
    Loaded,
    Failed(String),
}

/// OpenAI 互換プロバイダの編集状態。
pub struct ProviderSettingsModel {
    pub open: bool,
    pub name: String,
    pub base_url: String,
    pub api_key_env: String,
    pub models_text: String,
    pub default_model: String,
    pub error: Option<String>,
    pub excluded_models_text: String,
    pub available_models: Option<Vec<String>>,
    pub models_fetch_state: ModelsFetchState,
    pub models_rx: Option<Receiver<Result<Vec<String>, String>>>,
    pub models_fetch_base_url: Option<String>,
}

impl Default for ProviderSettingsModel {
    fn default() -> Self {
        Self {
            open: false,
            name: "openai-compat".into(),
            base_url: String::new(),
            api_key_env: String::new(),
            models_text: String::new(),
            default_model: String::new(),
            error: None,
            excluded_models_text: String::new(),
            available_models: None,
            models_fetch_state: ModelsFetchState::Idle,
            models_rx: None,
            models_fetch_base_url: None,
        }
    }
}

impl std::fmt::Debug for ProviderSettingsModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderSettingsModel")
            .field("open", &self.open)
            .field("name", &self.name)
            .field("base_url", &self.base_url)
            .field("api_key_env", &self.api_key_env)
            .field("models_text", &self.models_text)
            .field("default_model", &self.default_model)
            .field("error", &self.error)
            .field("excluded_models_text", &self.excluded_models_text)
            .field("available_models", &self.available_models)
            .field("models_fetch_state", &self.models_fetch_state)
            .field("models_fetch_base_url", &self.models_fetch_base_url)
            .field("models_rx", &self.models_rx.as_ref().map(|_| "..."))
            .finish()
    }
}

impl Clone for ProviderSettingsModel {
    fn clone(&self) -> Self {
        Self {
            open: self.open,
            name: self.name.clone(),
            base_url: self.base_url.clone(),
            api_key_env: self.api_key_env.clone(),
            models_text: self.models_text.clone(),
            default_model: self.default_model.clone(),
            error: self.error.clone(),
            excluded_models_text: self.excluded_models_text.clone(),
            available_models: self.available_models.clone(),
            models_fetch_state: self.models_fetch_state.clone(),
            models_fetch_base_url: self.models_fetch_base_url.clone(),
            models_rx: None,
        }
    }
}

impl PartialEq for ProviderSettingsModel {
    fn eq(&self, other: &Self) -> bool {
        self.open == other.open
            && self.name == other.name
            && self.base_url == other.base_url
            && self.api_key_env == other.api_key_env
            && self.models_text == other.models_text
            && self.default_model == other.default_model
            && self.error == other.error
            && self.excluded_models_text == other.excluded_models_text
            && self.available_models == other.available_models
            && self.models_fetch_state == other.models_fetch_state
            && self.models_fetch_base_url == other.models_fetch_base_url
    }
}

impl Eq for ProviderSettingsModel {}

impl ProviderSettingsModel {
    /// 名前順で最初の OpenAI 互換プロバイダから編集状態を作る。
    pub fn seed_from_config(config: &config::Config) -> Self {
        let Some((name, profile)) = config.providers.iter().find(|(_, profile)| {
            profile.provider_type == config::ProviderTypeConfig::OpenAiCompatible
        }) else {
            return Self::default();
        };
        let api_key_env = match &profile.credential {
            config::CredentialRefConfig::Env { var } => var.clone(),
            config::CredentialRefConfig::Keyring { .. } => String::new(),
        };
        Self {
            name: name.clone(),
            base_url: profile.base_url.clone(),
            api_key_env,
            models_text: profile.models.join("\n"),
            default_model: profile.default_model.clone(),
            excluded_models_text: profile.excluded_models.join("\n"),
            ..Self::default()
        }
    }

    /// 改行・カンマ区切りのモデル ID を初出順に正規化する。
    pub fn parsed_models(&self) -> Vec<String> {
        let mut models = Vec::new();
        for model in self.models_text.split(['\n', ',']).map(str::trim) {
            if !model.is_empty() && !models.iter().any(|existing| existing == model) {
                models.push(model.to_owned());
            }
        }
        models
    }

    /// 改行・カンマ区切りの除外モデル ID を初出順に正規化する。
    pub fn parsed_excluded_models(&self) -> Vec<String> {
        let mut models = Vec::new();
        for model in self.excluded_models_text.split(['\n', ',']).map(str::trim) {
            if !model.is_empty() && !models.iter().any(|existing| existing == model) {
                models.push(model.to_owned());
            }
        }
        models
    }

    /// 除外フィルタを適用し、選択中のモデルは末尾に補う。
    pub fn candidate_models(&self) -> Vec<String> {
        let mut choices = self
            .available_models
            .clone()
            .unwrap_or_else(|| self.parsed_models());
        let excluded = self.parsed_excluded_models();
        choices.retain(|id| !excluded.contains(id));
        if !self.default_model.is_empty() && !choices.contains(&self.default_model) {
            choices.push(self.default_model.clone());
        }
        choices
    }

    /// モデル一覧以外は入力をそのまま渡し、検証は config に委ねる。
    pub fn to_input(&self) -> config::OpenAiCompatibleProviderInput {
        let mut models = self.parsed_models();
        let default_model = self.default_model.trim();
        if !default_model.is_empty() && !models.iter().any(|id| id == default_model) {
            models.push(default_model.to_owned());
        }
        config::OpenAiCompatibleProviderInput {
            name: self.name.clone(),
            base_url: self.base_url.clone(),
            api_key_env: self.api_key_env.clone(),
            models,
            excluded_models: self.parsed_excluded_models(),
            default_model: self.default_model.clone(),
        }
    }

    /// /v1/models からモデル一覧を非同期に取得し、結果をチャネルへ送る。
    pub fn start_models_fetch(&mut self) {
        let api_key = std::env::var(&self.api_key_env)
            .ok()
            .filter(|key| !key.is_empty());
        self.start_models_fetch_with_key(api_key);
    }

    /// Dependency-injected fetch entry point for tests that must not mutate environment variables.
    pub fn start_models_fetch_with_key(&mut self, api_key: Option<String>) {
        self.models_fetch_state = ModelsFetchState::Loading;
        self.available_models = None;
        let base_url = self.base_url.clone();
        self.models_fetch_base_url = Some(base_url.clone());
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let api_key = match api_key {
                Some(key) => key,
                None => {
                    let _ = tx.send(Err("API key env var is not set".into()));
                    return;
                }
            };
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = tx.send(Err(error.to_string()));
                    return;
                }
            };
            let result = runtime.block_on(providers::list_models(
                &base_url,
                &providers::ProviderAuth::new(api_key),
            ));
            let _ = tx.send(result.map_err(|error| error.to_string()));
        });
        self.models_rx = Some(rx);
    }

    /// チャネルから取得結果を受け取り、状態を更新する。UI 再描画が必要なら true を返す。
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

/// プロバイダの登録有無から表示状態を判定する。
pub fn provider_status_of(config: &config::Config) -> ProviderStatus {
    if config.providers.is_empty() {
        ProviderStatus::NotConfigured {
            guidance: PROVIDER_MISSING_GUIDANCE.into(),
        }
    } else {
        ProviderStatus::Configured
    }
}

#[cfg(test)]
#[path = "provider_settings_candidates_tests.rs"]
mod candidates_tests;

#[cfg(test)]
#[path = "provider_settings_fetch_tests.rs"]
mod fetch_tests;

#[cfg(test)]
#[path = "provider_settings_tests.rs"]
mod tests;
