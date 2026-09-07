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

    /// モデル一覧以外は入力をそのまま渡し、検証は config に委ねる。
    pub fn to_input(&self) -> config::OpenAiCompatibleProviderInput {
        config::OpenAiCompatibleProviderInput {
            name: self.name.clone(),
            base_url: self.base_url.clone(),
            api_key_env: self.api_key_env.clone(),
            models: self.parsed_models(),
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
            Ok(Ok(models)) => {
                self.available_models = Some(models);
                self.models_fetch_state = ModelsFetchState::Loaded;
                true
            }
            Ok(Err(error)) => {
                self.available_models = None;
                self.models_fetch_state = ModelsFetchState::Failed(error);
                true
            }
            Err(TryRecvError::Empty) => {
                self.models_rx = Some(rx);
                false
            }
            Err(TryRecvError::Disconnected) => {
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
mod tests {
    use super::*;
    use config::{Config, CredentialRefConfig, ProviderProfileConfig, ProviderTypeConfig};

    fn compatible(credential: CredentialRefConfig) -> ProviderProfileConfig {
        ProviderProfileConfig {
            provider_type: ProviderTypeConfig::OpenAiCompatible,
            api_protocol: config::ApiProtocolConfig::OpenAiCompletions,
            base_url: "https://example.com/v1".into(),
            credential,
            models: vec!["model-b".into(), "model-a".into()],
            default_model: "model-a".into(),
            excluded_models: vec!["excluded-b".into(), "excluded-a".into()],
        }
    }

    #[test]
    fn seed_from_config_picks_first_openai_compatible_env_provider() {
        // Given
        let mut config = Config::default();
        config
            .providers
            .insert("a-anthropic".into(), ProviderProfileConfig::default());
        config.providers.insert(
            "z-compatible".into(),
            compatible(CredentialRefConfig::Env {
                var: "LATER_KEY".into(),
            }),
        );
        config.providers.insert(
            "b-compatible".into(),
            compatible(CredentialRefConfig::Env {
                var: "FIRST_KEY".into(),
            }),
        );
        // When
        let model = ProviderSettingsModel::seed_from_config(&config);
        // Then
        assert_eq!(
            model,
            ProviderSettingsModel {
                open: false,
                name: "b-compatible".into(),
                base_url: "https://example.com/v1".into(),
                api_key_env: "FIRST_KEY".into(),
                models_text: "model-b\nmodel-a".into(),
                default_model: "model-a".into(),
                error: None,
                excluded_models_text: "excluded-b\nexcluded-a".into(),
                available_models: None,
                models_fetch_state: ModelsFetchState::Idle,
                models_rx: None,
            }
        );
    }

    #[test]
    fn seed_from_config_with_keyring_credential_leaves_api_key_env_empty() {
        // Given
        let mut config = Config::default();
        config.providers.insert(
            "keyring".into(),
            compatible(CredentialRefConfig::Keyring {
                service: "service".into(),
                account: "account".into(),
            }),
        );
        // When
        let model = ProviderSettingsModel::seed_from_config(&config);
        // Then
        assert_eq!(model.name, "keyring");
        assert_eq!(model.api_key_env, "");
        assert_eq!(model.base_url, "https://example.com/v1");
        assert_eq!(model.models_text, "model-b\nmodel-a");
        assert_eq!(model.default_model, "model-a");
        assert_eq!(model.excluded_models_text, "excluded-b\nexcluded-a");
    }

    #[test]
    fn seed_from_config_without_openai_compatible_returns_default() {
        // Given
        let mut config = Config::default();
        config
            .providers
            .insert("anthropic".into(), ProviderProfileConfig::default());
        // When
        let model = ProviderSettingsModel::seed_from_config(&config);
        // Then
        assert_eq!(model, ProviderSettingsModel::default());
        assert_eq!(
            model,
            ProviderSettingsModel {
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
            }
        );
    }

    #[test]
    fn parsed_models_splits_trims_and_dedupes() {
        // Given
        let model = ProviderSettingsModel {
            models_text: " model-b, model-a\n\nmodel-b, , model-c\r\n model-a,\n".into(),
            ..ProviderSettingsModel::default()
        };
        // When
        let models = model.parsed_models();
        // Then
        assert_eq!(models, ["model-b", "model-a", "model-c"]);
    }

    #[test]
    fn parsed_excluded_models_splits_trims_and_dedupes() {
        // Given
        let model = ProviderSettingsModel {
            excluded_models_text: " ex-b, ex-a\n\nex-b, , ex-c\r\n ex-a,\n".into(),
            ..ProviderSettingsModel::default()
        };
        // When
        let excluded = model.parsed_excluded_models();
        // Then
        assert_eq!(excluded, ["ex-b", "ex-a", "ex-c"]);
    }

    #[test]
    fn to_input_uses_parsed_models_and_raw_fields() {
        // Given
        let model = ProviderSettingsModel {
            name: " raw-name ".into(),
            base_url: " https://example.com/v1 ".into(),
            api_key_env: " API_KEY ".into(),
            default_model: " model-b ".into(),
            models_text: " model-b,model-a\nmodel-b ".into(),
            excluded_models_text: " ex-a, ex-b\nex-a ".into(),
            ..ProviderSettingsModel::default()
        };
        // When
        let input = model.to_input();
        // Then
        assert_eq!(input.name, " raw-name ");
        assert_eq!(input.base_url, " https://example.com/v1 ");
        assert_eq!(input.api_key_env, " API_KEY ");
        assert_eq!(input.default_model, " model-b ");
        assert_eq!(input.models, ["model-b", "model-a"]);
        assert_eq!(input.excluded_models, ["ex-a", "ex-b"]);
    }

    #[test]
    fn seed_from_config_round_trips_excluded_models() {
        // Given
        let mut config = Config::default();
        config.providers.insert(
            "roundtrip".into(),
            compatible(CredentialRefConfig::Env { var: "KEY".into() }),
        );
        // When
        let model = ProviderSettingsModel::seed_from_config(&config);
        let input = model.to_input();
        // Then
        assert_eq!(input.excluded_models, ["excluded-b", "excluded-a"]);
    }

    #[test]
    fn poll_models_transitions_to_loaded_on_success() {
        // Given
        let (tx, rx) = channel();
        let mut model = ProviderSettingsModel {
            models_rx: Some(rx),
            models_fetch_state: ModelsFetchState::Loading,
            ..ProviderSettingsModel::default()
        };
        tx.send(Ok(vec!["fetched-a".into(), "fetched-b".into()]))
            .unwrap();
        // When
        let changed = model.poll_models();
        // Then
        assert!(changed);
        assert_eq!(model.models_fetch_state, ModelsFetchState::Loaded);
        assert_eq!(
            model.available_models,
            Some(vec!["fetched-a".into(), "fetched-b".into()])
        );
        assert!(model.models_rx.is_none());
    }

    #[test]
    fn poll_models_transitions_to_failed_on_error() {
        // Given
        let (tx, rx) = channel();
        let mut model = ProviderSettingsModel {
            models_rx: Some(rx),
            models_fetch_state: ModelsFetchState::Loading,
            ..ProviderSettingsModel::default()
        };
        tx.send(Err("network error".into())).unwrap();
        // When
        let changed = model.poll_models();
        // Then
        assert!(changed);
        assert_eq!(
            model.models_fetch_state,
            ModelsFetchState::Failed("network error".into())
        );
        assert_eq!(model.available_models, None);
        assert!(model.models_rx.is_none());
    }

    #[test]
    fn poll_models_returns_false_when_channel_is_empty() {
        // Given
        let (_tx, rx) = channel();
        let mut model = ProviderSettingsModel {
            models_rx: Some(rx),
            models_fetch_state: ModelsFetchState::Loading,
            ..ProviderSettingsModel::default()
        };
        // When
        let changed = model.poll_models();
        // Then
        assert!(!changed);
        assert_eq!(model.models_fetch_state, ModelsFetchState::Loading);
        assert!(model.models_rx.is_some());
    }

    #[test]
    fn provider_status_of_empty_providers_is_not_configured() {
        // Given
        let config = Config::default();
        // When
        let status = provider_status_of(&config);
        // Then
        assert_eq!(
            status,
            ProviderStatus::NotConfigured {
                guidance: PROVIDER_MISSING_GUIDANCE.into()
            }
        );
    }

    #[test]
    fn provider_status_of_non_empty_providers_is_configured() {
        // Given
        let mut config = Config::default();
        config
            .providers
            .insert("anthropic".into(), ProviderProfileConfig::default());
        // When
        let status = provider_status_of(&config);
        // Then
        assert_eq!(status, ProviderStatus::Configured);
    }
}
