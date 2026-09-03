//! 設定ファイルのルート構造と各セクション型の再エクスポートを行います。

pub mod agents;
pub mod misc;
pub mod panel;
pub mod provider;
pub mod routing;
pub mod rules;

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub use agents::{
    AgentsConfig, CategoryBindingConfig, GenerationOverridesConfig, ReasoningEffortConfig,
    ResolvedAgentBinding, RoleBindingConfig,
};
pub use misc::{DiagnosticsConfig, MetricsConfig, PermissionConfig};
pub use panel::PanelConfig;
pub use provider::{
    ApiProtocolConfig, CredentialRefConfig, ProviderProfileConfig, ProviderTypeConfig,
};
pub use routing::{RouteCandidateConfig, RoutingConfig};
pub use rules::RulesConfig;

/// 現在の設定スキーマバージョン (ADR 0014)。
pub const CURRENT_VERSION: u32 = 2;

/// 設定ファイルのルート構造。
///
/// 各セクションは [`serde(default)`] により省略可能ですが、未知のキーは拒否します。
/// ロード経路では [`crate::strict`] がドット区切りの設定パスを報告し、直接の serde
/// パースでも `deny_unknown_fields` により拒否します。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// 設定スキーマのバージョン。
    pub version: u32,
    /// プロバイダプロファイル (マップキーがプロファイル名)。
    pub providers: BTreeMap<String, ProviderProfileConfig>,
    /// ロール別エージェントバインディング。
    pub agents: AgentsConfig,
    /// ルーティング設定。
    pub routing: RoutingConfig,
    /// パネル UI 設定。
    pub panel: PanelConfig,
    /// 診断 (ログ) 設定。
    pub diagnostics: DiagnosticsConfig,
    /// 権限設定。
    pub permissions: PermissionConfig,
    /// メトリクス設定。
    pub metrics: MetricsConfig,
    /// プロジェクトルール注入設定。
    pub rules: RulesConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            providers: BTreeMap::new(),
            agents: AgentsConfig::default(),
            routing: RoutingConfig::default(),
            panel: PanelConfig::default(),
            diagnostics: DiagnosticsConfig::default(),
            permissions: PermissionConfig::default(),
            metrics: MetricsConfig::default(),
            rules: RulesConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given: バージョン 2 の全セクションを含む設定 TOML / When: Config にパースする
    // Then: 各値が意図どおり読み取れる
    #[test]
    fn deserialize_full_config_toml() {
        let doc = r#"
version = 2

[providers.anthropic-main]
provider_type = "anthropic"
api_protocol = "anthropic-messages"
base_url = "https://api.anthropic.com"
credential = { type = "keyring", service = "evorch", account = "anthropic-main" }
models = ["claude-sonnet-4-5", "claude-opus-4-1"]
default_model = "claude-sonnet-4-5"

[providers.openrouter-main]
provider_type = "openrouter"
api_protocol = "openai-completions"
base_url = "https://openrouter.ai/api/v1"
credential = { type = "env", var = "OPENROUTER_API_KEY" }
models = ["gpt-5.2"]
default_model = "gpt-5.2"

[[routing.routes.fast]]
profile = "anthropic-main"
model = "claude-opus-4-1"

[[routing.routes.fast]]
profile = "openrouter-main"

[[routing.routes.cheap]]
profile = "openrouter-main"

[panel]
layout = "compact"

[panel.keybinds]
quit = "q"
new_task = "n"

[diagnostics]
log_level = "debug"
log_dir = "/tmp/evorch-logs"

[permissions]
preset = "strict"

[metrics]
enabled = false
retention_days = 90
"#;

        let config: Config = toml::from_str(doc).expect("フル設定をパースできる");

        assert_eq!(config.version, 2);

        let anthropic = config
            .providers
            .get("anthropic-main")
            .expect("anthropic-main プロファイルが存在する");
        assert_eq!(anthropic.provider_type, ProviderTypeConfig::Anthropic);
        assert_eq!(anthropic.api_protocol, ApiProtocolConfig::AnthropicMessages);
        assert_eq!(anthropic.base_url, "https://api.anthropic.com");
        assert_eq!(
            anthropic.credential,
            CredentialRefConfig::Keyring {
                service: "evorch".to_string(),
                account: "anthropic-main".to_string(),
            }
        );
        assert_eq!(anthropic.models, ["claude-sonnet-4-5", "claude-opus-4-1"]);
        assert_eq!(anthropic.default_model, "claude-sonnet-4-5");

        let openrouter = config
            .providers
            .get("openrouter-main")
            .expect("openrouter-main プロファイルが存在する");
        assert_eq!(openrouter.provider_type, ProviderTypeConfig::Openrouter);
        assert_eq!(
            openrouter.api_protocol,
            ApiProtocolConfig::OpenAiCompletions
        );
        assert_eq!(
            openrouter.credential,
            CredentialRefConfig::Env {
                var: "OPENROUTER_API_KEY".to_string(),
            }
        );

        let fast = config
            .routing
            .routes
            .get("fast")
            .expect("fast ルートが存在する");
        assert_eq!(fast.len(), 2);
        assert_eq!(fast[0].profile, "anthropic-main");
        assert_eq!(fast[0].model.as_deref(), Some("claude-opus-4-1"));
        assert_eq!(fast[1].profile, "openrouter-main");
        assert_eq!(fast[1].model, None);

        let cheap = config
            .routing
            .routes
            .get("cheap")
            .expect("cheap ルートが存在する");
        assert_eq!(cheap.len(), 1);
        assert_eq!(cheap[0].profile, "openrouter-main");
        assert_eq!(cheap[0].model, None);

        assert_eq!(config.panel.layout, "compact");
        assert_eq!(
            config.panel.keybinds.get("quit").map(String::as_str),
            Some("q")
        );
        assert_eq!(
            config.panel.keybinds.get("new_task").map(String::as_str),
            Some("n")
        );

        assert_eq!(config.diagnostics.log_level, "debug");
        assert_eq!(
            config.diagnostics.log_dir.as_deref(),
            Some("/tmp/evorch-logs")
        );
        assert_eq!(config.permissions.preset, "strict");
        assert!(!config.metrics.enabled);
        assert_eq!(config.metrics.retention_days, 90);
    }

    // Given: 既定の Config / When: TOML に直列化して再度パースする
    // Then: 元の設定と等しく、バージョンは CURRENT_VERSION
    #[test]
    fn default_config_serializes_and_reparses() {
        let config = Config::default();
        assert_eq!(config.version, CURRENT_VERSION);

        let serialized = toml::to_string(&config).expect("既定設定を TOML に直列化できる");
        let reparsed: Config = toml::from_str(&serialized).expect("直列化結果をパースできる");

        assert_eq!(reparsed, config);
    }

    // Given: ルートに不明なキーを含む設定 TOML / When: Config にパースする
    // Then: エラーとして拒否される
    #[test]
    fn unknown_root_field_rejected() {
        let doc = "version = 2\nunknown_key = 1";

        let result = toml::from_str::<Config>(doc);

        assert!(
            result.is_err(),
            "ルートの不明なキーは拒否される: {result:?}"
        );
    }

    // Given: 不明な provider_type 値を含む TOML / When: パースを試みる
    // Then: エラーとして拒否される
    #[test]
    fn unknown_provider_type_value_is_rejected() {
        let doc = r#"
provider_type = "not-a-provider"
api_protocol = "anthropic-messages"
base_url = "https://api.anthropic.com"
models = []
default_model = "claude-sonnet-4-5"

[credential]
type = "env"
var = "ANTHROPIC_API_KEY"
"#;

        let result = toml::from_str::<ProviderProfileConfig>(doc);
        assert!(
            result.is_err(),
            "不明な provider_type は拒否される: {result:?}"
        );
    }
}
