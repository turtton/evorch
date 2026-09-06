//! egui に依存しないプロバイダ設定の編集モデル。

use super::composer::{PROVIDER_MISSING_GUIDANCE, ProviderStatus};

/// OpenAI 互換プロバイダの編集状態。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderSettingsModel {
    pub open: bool,
    pub name: String,
    pub base_url: String,
    pub api_key_env: String,
    pub models_text: String,
    pub default_model: String,
    pub error: Option<String>,
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
        }
    }
}

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

    /// モデル一覧以外は入力をそのまま渡し、検証は config に委ねる。
    pub fn to_input(&self) -> config::OpenAiCompatibleProviderInput {
        config::OpenAiCompatibleProviderInput {
            name: self.name.clone(),
            base_url: self.base_url.clone(),
            api_key_env: self.api_key_env.clone(),
            models: self.parsed_models(),
            default_model: self.default_model.clone(),
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
    fn to_input_uses_parsed_models_and_raw_fields() {
        // Given
        let model = ProviderSettingsModel {
            name: " raw-name ".into(),
            base_url: " https://example.com/v1 ".into(),
            api_key_env: " API_KEY ".into(),
            default_model: " model-b ".into(),
            models_text: " model-b,model-a\nmodel-b ".into(),
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
