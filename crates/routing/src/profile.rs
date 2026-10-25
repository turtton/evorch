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
    /// 設定で明示された編集ツールの上書き。解決は [`ProviderProfile::edit_tool_for`] を使う。
    pub edit_tool: config::EditToolSelection,
}

impl ProviderProfile {
    /// `model_id` (プロファイル名を除いたモデル ID) に提示する編集ツールを返します。
    ///
    /// model entry > プロファイル > 既定判定の順で、既定判定にはこのプロファイルの
    /// `provider_type` を使います。
    pub fn edit_tool_for(&self, model_id: &str) -> config::EditToolConfig {
        self.edit_tool
            .for_model(provider_type_config(self.provider_type), model_id)
    }
}

fn provider_type_config(provider_type: model::ProviderType) -> config::ProviderTypeConfig {
    match provider_type {
        model::ProviderType::Cursor => config::ProviderTypeConfig::Cursor,
        model::ProviderType::Anthropic => config::ProviderTypeConfig::Anthropic,
        model::ProviderType::AnthropicSubscription => {
            config::ProviderTypeConfig::AnthropicSubscription
        }
        model::ProviderType::OpenAi => config::ProviderTypeConfig::OpenAi,
        model::ProviderType::OpenAiCodex => config::ProviderTypeConfig::OpenAiCodex,
        model::ProviderType::GithubCopilot => config::ProviderTypeConfig::GithubCopilot,
        model::ProviderType::Openrouter => config::ProviderTypeConfig::Openrouter,
        model::ProviderType::OpenAiCompatible => config::ProviderTypeConfig::OpenAiCompatible,
        model::ProviderType::KimiSubscription => config::ProviderTypeConfig::KimiSubscription,
    }
}

impl TryFrom<(&str, &config::ProviderProfileConfig)> for ProviderProfile {
    type Error = RoutingError;

    /// 設定プロファイルをルーティング用の検証済みプロファイルへ変換します。
    ///
    /// # Errors
    /// 有効なモデルがない、`default_model` が無効または不在、または `base_url` が空の場合に
    /// [`RoutingError::InvalidProfile`] を返します。
    fn try_from(
        (name, config): (&str, &config::ProviderProfileConfig),
    ) -> Result<Self, Self::Error> {
        let models = config.enabled_model_ids();
        if models.is_empty() {
            return Err(RoutingError::InvalidProfile {
                reason: "models must contain at least one enabled model".to_string(),
            });
        }
        if !models.contains(&config.default_model) {
            return Err(RoutingError::InvalidProfile {
                reason: "default_model must be an enabled model".to_string(),
            });
        }
        if config.base_url.is_empty() {
            return Err(RoutingError::InvalidProfile {
                reason: "base_url must not be empty".to_string(),
            });
        }

        let provider_type = match config.provider_type {
            config::ProviderTypeConfig::Cursor => model::ProviderType::Cursor,
            config::ProviderTypeConfig::Anthropic => model::ProviderType::Anthropic,
            config::ProviderTypeConfig::AnthropicSubscription => {
                model::ProviderType::AnthropicSubscription
            }
            config::ProviderTypeConfig::OpenAi => model::ProviderType::OpenAi,
            config::ProviderTypeConfig::OpenAiCodex => model::ProviderType::OpenAiCodex,
            config::ProviderTypeConfig::GithubCopilot => model::ProviderType::GithubCopilot,
            config::ProviderTypeConfig::Openrouter => model::ProviderType::Openrouter,
            config::ProviderTypeConfig::OpenAiCompatible => model::ProviderType::OpenAiCompatible,
            config::ProviderTypeConfig::KimiSubscription => model::ProviderType::KimiSubscription,
        };
        let api_protocol = match config.api_protocol {
            config::ApiProtocolConfig::CursorAgent => model::ApiProtocol::CursorAgent,
            config::ApiProtocolConfig::AnthropicMessages => model::ApiProtocol::AnthropicMessages,
            config::ApiProtocolConfig::OpenAiResponses => model::ApiProtocol::OpenAiResponses,
            config::ApiProtocolConfig::OpenAiCompletions => model::ApiProtocol::OpenAiCompletions,
            config::ApiProtocolConfig::OpenAiCodexResponses => {
                model::ApiProtocol::OpenAiCodexResponses
            }
        };

        Ok(Self {
            name: name.to_string(),
            provider_type,
            api_protocol,
            base_url: config.base_url.clone(),
            credential: CredentialRef::from(&config.credential),
            models,
            default_model: config.default_model.clone(),
            edit_tool: config.edit_tool_selection(),
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
            models: vec![
                config::types::provider::ModelEntryConfig::enabled("model-a"),
                config::types::provider::ModelEntryConfig::enabled("model-b"),
            ],
            excluded_models: Vec::new(),
            default_model: "model-b".to_string(),
            edit_tool: None,
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

    // Given: Codex provider と Codex Responses protocol の設定 / When: ProviderProfile に変換する
    // Then: Codex 専用の protocol variant が保持される
    #[test]
    fn provider_profile_maps_codex_responses_protocol() {
        let config = config::ProviderProfileConfig {
            provider_type: config::ProviderTypeConfig::OpenAiCodex,
            api_protocol: config::ApiProtocolConfig::OpenAiCodexResponses,
            ..valid_config()
        };

        let profile =
            ProviderProfile::try_from(("codex", &config)).expect("Codex 設定は変換できる");

        assert_eq!(profile.provider_type, model::ProviderType::OpenAiCodex);
        assert_eq!(
            profile.api_protocol,
            model::ApiProtocol::OpenAiCodexResponses
        );
    }

    #[test]
    fn provider_profile_maps_kimi_subscription_type() {
        let config = config::ProviderProfileConfig {
            provider_type: config::ProviderTypeConfig::KimiSubscription,
            api_protocol: config::ApiProtocolConfig::OpenAiCompletions,
            ..valid_config()
        };

        let profile = ProviderProfile::try_from(("kimi", &config)).expect("kimi 設定は変換できる");

        assert_eq!(profile.provider_type, model::ProviderType::KimiSubscription);
        assert_eq!(profile.api_protocol, model::ApiProtocol::OpenAiCompletions);
    }

    #[test]
    fn disabled_models_are_excluded_from_profile() {
        let mut config = valid_config();
        config.models[0].enabled = false;

        let profile = ProviderProfile::try_from(("primary", &config)).expect("enabled default");

        assert_eq!(profile.models, ["model-b"]);
    }

    #[test]
    fn default_model_disabled_errors() {
        let mut config = valid_config();
        config.models[1].enabled = false;

        let error = ProviderProfile::try_from(("primary", &config)).expect_err("disabled default");

        assert_eq!(
            error,
            RoutingError::InvalidProfile {
                reason: "default_model must be an enabled model".to_string()
            }
        );
    }

    #[test]
    fn all_models_disabled_errors() {
        let mut config = valid_config();
        for entry in &mut config.models {
            entry.enabled = false;
        }

        let error = ProviderProfile::try_from(("primary", &config)).expect_err("no enabled models");

        assert_eq!(
            error,
            RoutingError::InvalidProfile {
                reason: "models must contain at least one enabled model".to_string()
            }
        );
    }

    #[test]
    fn enabled_models_are_preserved_in_declaration_order() {
        let mut config = valid_config();
        config.models = vec![
            config::types::provider::ModelEntryConfig::enabled("model-z"),
            config::types::provider::ModelEntryConfig {
                enabled: false,
                ..config::types::provider::ModelEntryConfig::enabled("model-disabled")
            },
            config::types::provider::ModelEntryConfig::enabled("model-b"),
            config::types::provider::ModelEntryConfig::enabled("model-a"),
        ];

        let profile = ProviderProfile::try_from(("primary", &config)).expect("enabled models");

        assert_eq!(profile.models, ["model-z", "model-b", "model-a"]);
    }

    // Given: profile と model entry に edit_tool を書いた設定 / When: ProviderProfile に変換する
    // Then: 上書きが受け渡され、model > profile の順に解決される
    #[test]
    fn provider_profile_carries_edit_tool_overrides() {
        let mut config = valid_config();
        config.edit_tool = Some(config::EditToolConfig::Edit);
        config.models[0].edit_tool = Some(config::EditToolConfig::ApplyPatch);

        let profile = ProviderProfile::try_from(("primary", &config)).expect("edit tool");

        assert_eq!(
            profile.edit_tool,
            config::EditToolSelection {
                profile: Some(config::EditToolConfig::Edit),
                models: [("model-a".to_owned(), config::EditToolConfig::ApplyPatch)].into(),
            }
        );
        assert_eq!(
            profile.edit_tool_for("model-a"),
            config::EditToolConfig::ApplyPatch
        );
        assert_eq!(
            profile.edit_tool_for("model-a+fast"),
            config::EditToolConfig::ApplyPatch
        );
        assert_eq!(
            profile.edit_tool_for("model-b"),
            config::EditToolConfig::Edit
        );
    }

    // Given: edit_tool を書かない Codex / OpenAI 互換の設定 / When: 変換して解決する
    // Then: 各プロファイルの provider_type で既定判定する
    #[test]
    fn provider_profile_defaults_edit_tool_by_provider_type() {
        let codex = config::ProviderProfileConfig {
            provider_type: config::ProviderTypeConfig::OpenAiCodex,
            api_protocol: config::ApiProtocolConfig::OpenAiCodexResponses,
            models: vec![config::types::provider::ModelEntryConfig::enabled(
                "gpt-6-astra",
            )],
            default_model: "gpt-6-astra".to_string(),
            ..valid_config()
        };
        let compatible = config::ProviderProfileConfig {
            provider_type: config::ProviderTypeConfig::OpenAiCompatible,
            ..codex.clone()
        };

        let codex = ProviderProfile::try_from(("codex", &codex)).expect("codex");
        let compatible = ProviderProfile::try_from(("compat", &compatible)).expect("compatible");

        assert_eq!(codex.edit_tool, config::EditToolSelection::default());
        assert_eq!(
            codex.edit_tool_for("gpt-6-astra"),
            config::EditToolConfig::ApplyPatch
        );
        assert_eq!(
            compatible.edit_tool_for("gpt-6-astra"),
            config::EditToolConfig::Edit
        );
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
                reason: "models must contain at least one enabled model".to_string()
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
                reason: "default_model must be an enabled model".to_string()
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
