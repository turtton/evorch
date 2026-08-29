use crate::{CredentialRef, RoutingError};

/// ルーティングで使用するプロバイダの検証済みプロファイルです。
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderProfile {
    /// 設定上のプロファイル名。
    pub name: String,
    /// プロバイダの種別。
    pub provider_type: model::ProviderType,
    /// モデルとの通信に用いる API プロトコル。
    pub api_protocol: model::ApiProtocol,
    /// API のベース URL。
    pub base_url: String,
    /// 認証情報の取得先。
    pub credential: CredentialRef,
    /// 利用可能なモデル ID。
    pub models: Vec<String>,
    /// 既定で使用するモデル ID。
    pub default_model: String,
}

impl TryFrom<(&str, &config::ProviderProfileConfig)> for ProviderProfile {
    type Error = RoutingError;

    /// 設定プロファイルをルーティング用の検証済みプロファイルへ変換します。
    ///
    /// # Errors
    /// `models` が空、`default_model` が `models` に含まれない、または `base_url` が空の場合に
    /// [`RoutingError::InvalidProfile`] を返します。
    fn try_from(
        (name, config): (&str, &config::ProviderProfileConfig),
    ) -> Result<Self, Self::Error> {
        if config.models.is_empty() {
            return Err(RoutingError::InvalidProfile {
                reason: "models must not be empty".to_string(),
            });
        }
        if !config.models.contains(&config.default_model) {
            return Err(RoutingError::InvalidProfile {
                reason: "default_model must be included in models".to_string(),
            });
        }
        if config.base_url.is_empty() {
            return Err(RoutingError::InvalidProfile {
                reason: "base_url must not be empty".to_string(),
            });
        }

        let provider_type = match config.provider_type {
            config::ProviderTypeConfig::Anthropic => model::ProviderType::Anthropic,
            config::ProviderTypeConfig::AnthropicSubscription => {
                model::ProviderType::AnthropicSubscription
            }
            config::ProviderTypeConfig::OpenAi => model::ProviderType::OpenAi,
            config::ProviderTypeConfig::OpenAiCodex => model::ProviderType::OpenAiCodex,
            config::ProviderTypeConfig::GithubCopilot => model::ProviderType::GithubCopilot,
            config::ProviderTypeConfig::Openrouter => model::ProviderType::Openrouter,
            config::ProviderTypeConfig::OpenAiCompatible => model::ProviderType::OpenAiCompatible,
        };
        let api_protocol = match config.api_protocol {
            config::ApiProtocolConfig::AnthropicMessages => model::ApiProtocol::AnthropicMessages,
            config::ApiProtocolConfig::OpenAiResponses => model::ApiProtocol::OpenAiResponses,
            config::ApiProtocolConfig::OpenAiCompletions => model::ApiProtocol::OpenAiCompletions,
        };

        Ok(Self {
            name: name.to_string(),
            provider_type,
            api_protocol,
            base_url: config.base_url.clone(),
            credential: CredentialRef::from(&config.credential),
            models: config.models.clone(),
            default_model: config.default_model.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::ProviderProfile;
    use crate::{CredentialRef, RoutingError};

    fn valid_config() -> config::ProviderProfileConfig {
        config::ProviderProfileConfig {
            provider_type: config::ProviderTypeConfig::OpenAi,
            api_protocol: config::ApiProtocolConfig::OpenAiResponses,
            base_url: "https://api.example.test".to_string(),
            credential: config::CredentialRefConfig::Env {
                var: "API_KEY".to_string(),
            },
            models: vec!["model-a".to_string(), "model-b".to_string()],
            default_model: "model-b".to_string(),
        }
    }

    // Given: 完全なプロバイダ設定 / When: ProviderProfile に変換する / Then: 全フィールドが型付きで保持される
    #[test]
    fn provider_profile_from_valid_config_succeeds() {
        let config = valid_config();

        let profile =
            ProviderProfile::try_from(("primary", &config)).expect("有効な設定は変換できる");

        assert_eq!(profile.name, "primary");
        assert_eq!(profile.provider_type, model::ProviderType::OpenAi);
        assert_eq!(profile.api_protocol, model::ApiProtocol::OpenAiResponses);
        assert_eq!(profile.base_url, "https://api.example.test");
        assert_eq!(
            profile.credential,
            CredentialRef::Env {
                var: "API_KEY".to_string()
            }
        );
        assert_eq!(profile.models, ["model-a", "model-b"]);
        assert_eq!(profile.default_model, "model-b");
    }

    // Given: モデル一覧が空の設定 / When: ProviderProfile に変換する / Then: InvalidProfile を返す
    #[test]
    fn provider_profile_rejects_empty_models() {
        let mut config = valid_config();
        config.models = Vec::new();

        let error =
            ProviderProfile::try_from(("primary", &config)).expect_err("空のモデル一覧は不正");

        assert_eq!(
            error,
            RoutingError::InvalidProfile {
                reason: "models must not be empty".to_string()
            }
        );
    }

    // Given: 既定モデルがモデル一覧に無い設定 / When: ProviderProfile に変換する / Then: InvalidProfile を返す
    #[test]
    fn provider_profile_rejects_unknown_default_model() {
        let mut config = valid_config();
        config.default_model = "missing".to_string();

        let error =
            ProviderProfile::try_from(("primary", &config)).expect_err("既定モデル不在は不正");

        assert_eq!(
            error,
            RoutingError::InvalidProfile {
                reason: "default_model must be included in models".to_string()
            }
        );
    }

    // Given: ベースURLが空の設定 / When: ProviderProfile に変換する / Then: InvalidProfile を返す
    #[test]
    fn provider_profile_rejects_empty_base_url() {
        let mut config = valid_config();
        config.base_url = String::new();

        let error = ProviderProfile::try_from(("primary", &config)).expect_err("空のURLは不正");

        assert_eq!(
            error,
            RoutingError::InvalidProfile {
                reason: "base_url must not be empty".to_string()
            }
        );
    }
}
