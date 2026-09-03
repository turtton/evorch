//! プロバイダプロファイルに関する設定型を定義します。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// プロバイダの種別 (設定ファイル上の表現)。
///
/// シリアライズ識別子はケバブケース (例: `anthropic-subscription`) です。
/// `OpenAi` 系は語を分割しない識別子 (`openai`・`openai-codex`・
/// `openai-compatible`) として直列化するため個別に rename します (ADR 0004)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderTypeConfig {
    /// Anthropic API (従量課金)。
    #[default]
    Anthropic,
    /// Anthropic サブスクリプション (Claude Pro / Max)。
    AnthropicSubscription,
    /// OpenAI API。
    #[serde(rename = "openai")]
    OpenAi,
    /// OpenAI Codex (ChatGPT サブスクリプション連携)。
    #[serde(rename = "openai-codex")]
    OpenAiCodex,
    /// GitHub Copilot。
    GithubCopilot,
    /// OpenRouter。
    Openrouter,
    /// OpenAI 互換 API (汎用プレースホルダ)。
    #[serde(rename = "openai-compatible")]
    OpenAiCompatible,
}

/// モデルとの通信に用いる API プロトコル (設定ファイル上の表現)。
///
/// シリアライズ識別子はケバブケース (例: `anthropic-messages`) です。
/// `OpenAi` 系は [`ProviderTypeConfig`] と同様に語を分割しない識別子
/// (`openai-responses`・`openai-completions`・`openai-codex-responses`) として直列化します (ADR 0004)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ApiProtocolConfig {
    /// Anthropic Messages API。
    #[default]
    AnthropicMessages,
    /// OpenAI Responses API。
    #[serde(rename = "openai-responses")]
    OpenAiResponses,
    /// OpenAI Chat Completions API。
    #[serde(rename = "openai-completions")]
    OpenAiCompletions,
    /// OpenAI Codex Responses API。Codex subscription backend は `store=false` と `stream=true` を強制する。
    #[serde(rename = "openai-codex-responses")]
    OpenAiCodexResponses,
}

/// 認証情報の参照方法。
///
/// 秘密情報そのものは一切保持せず、取得先 (キーリングのサービス名・アカウント名、
/// または環境変数名) のみを表現します。この型が秘密素材を含まないことは
/// このクレートのハードな契約です。
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum CredentialRefConfig {
    /// OS のキーリングから取得する。
    Keyring {
        /// キーリングのサービス名。
        service: String,
        /// キーリングのアカウント名。
        account: String,
    },
    /// 環境変数から取得する。
    Env {
        /// 環境変数名。
        var: String,
    },
}

// serde は内部タグ付き enum で未知フィールドを拒否できないため、秘密素材を含まない
// ハードな契約を各 variant の厳格なミラー構造体で保証する。
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
enum CredentialRefDe {
    Keyring(KeyringRefDe),
    Env(EnvRefDe),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyringRefDe {
    service: String,
    account: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvRefDe {
    var: String,
}

impl From<CredentialRefDe> for CredentialRefConfig {
    fn from(value: CredentialRefDe) -> Self {
        match value {
            CredentialRefDe::Keyring(KeyringRefDe { service, account }) => {
                Self::Keyring { service, account }
            }
            CredentialRefDe::Env(EnvRefDe { var }) => Self::Env { var },
        }
    }
}

impl<'de> Deserialize<'de> for CredentialRefConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        CredentialRefDe::deserialize(deserializer).map(Into::into)
    }
}

impl Default for CredentialRefConfig {
    fn default() -> Self {
        Self::Env {
            var: "ANTHROPIC_API_KEY".to_string(),
        }
    }
}

/// プロバイダプロファイル 1 件分の設定。
///
/// [`super::Config`] の `providers` マップのキーがプロファイル名になります。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderProfileConfig {
    /// プロバイダの種別。
    pub provider_type: ProviderTypeConfig,
    /// 通信に用いる API プロトコル。
    pub api_protocol: ApiProtocolConfig,
    /// API のベース URL。
    pub base_url: String,
    /// 認証情報の参照。
    pub credential: CredentialRefConfig,
    /// 利用可能なモデル ID の一覧。
    pub models: Vec<String>,
    /// 既定で使用するモデル ID。
    pub default_model: String,
}

impl Default for ProviderProfileConfig {
    fn default() -> Self {
        Self {
            provider_type: ProviderTypeConfig::default(),
            api_protocol: ApiProtocolConfig::default(),
            base_url: "https://api.anthropic.com".to_string(),
            credential: CredentialRefConfig::default(),
            models: vec!["claude-sonnet-4-5".to_string()],
            default_model: "claude-sonnet-4-5".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given: keyring 参照と env 参照の 2 変異 / When: TOML に直列化して読み戻す
    // Then: type タグを保ったまま往復する
    #[test]
    fn credential_ref_keyring_and_env_roundtrip() {
        let keyring = CredentialRefConfig::Keyring {
            service: "evorch".to_string(),
            account: "anthropic-main".to_string(),
        };
        let keyring_toml = toml::to_string(&keyring).expect("keyring 参照を直列化できる");
        assert!(
            keyring_toml.contains("type = \"keyring\""),
            "type タグが keyring である: {keyring_toml}"
        );
        let keyring_parsed: CredentialRefConfig =
            toml::from_str(&keyring_toml).expect("keyring 参照を解析できる");
        assert_eq!(keyring_parsed, keyring);

        let env = CredentialRefConfig::Env {
            var: "OPENROUTER_API_KEY".to_string(),
        };
        let env_toml = toml::to_string(&env).expect("env 参照を直列化できる");
        assert!(
            env_toml.contains("type = \"env\""),
            "type タグが env である: {env_toml}"
        );
        let env_parsed: CredentialRefConfig =
            toml::from_str(&env_toml).expect("env 参照を解析できる");
        assert_eq!(env_parsed, env);
    }

    // Given: keyring 参照に不明な value フィールドを含む TOML / When: CredentialRefConfig にパースする
    // Then: エラーとして拒否される
    #[test]
    fn credential_keyring_extra_field_rejected() {
        let doc = "type = \"keyring\"\nservice = \"evorch\"\naccount = \"a\"\nvalue = \"x\"";

        let result = toml::from_str::<CredentialRefConfig>(doc);

        assert!(
            result.is_err(),
            "keyring の不明なフィールドは拒否される: {result:?}"
        );
    }

    // Given: env 参照に不明な api_key フィールドを含む TOML / When: CredentialRefConfig にパースする
    // Then: エラーとして拒否される
    #[test]
    fn credential_env_extra_field_rejected() {
        let doc = "type = \"env\"\nvar = \"V\"\napi_key = \"x\"";

        let result = toml::from_str::<CredentialRefConfig>(doc);

        assert!(
            result.is_err(),
            "env の不明なフィールドは拒否される: {result:?}"
        );
    }

    // Given: 不明なフィールドを含む完全なプロバイダプロファイル TOML / When: パースする
    // Then: エラーとして拒否される
    #[test]
    fn provider_profile_unknown_field_rejected() {
        let doc = r#"
provider_type = "anthropic"
api_protocol = "anthropic-messages"
base_url = "https://api.anthropic.com"
credential = { type = "env", var = "ANTHROPIC_API_KEY" }
models = ["claude-sonnet-4-5"]
default_model = "claude-sonnet-4-5"
unknown_extra = true
"#;

        let result = toml::from_str::<ProviderProfileConfig>(doc);

        assert!(
            result.is_err(),
            "プロファイルの不明なフィールドは拒否される: {result:?}"
        );
    }

    // Given: Codex Responses を指定するプロバイダプロファイル / When: TOML と JSON を往復する
    // Then: api_protocol の識別子と variant が保持される
    #[test]
    fn provider_profile_codex_responses_protocol_roundtrip() {
        let toml = r#"
provider_type = "openai-codex"
api_protocol = "openai-codex-responses"
base_url = "https://chatgpt.com/backend-api/codex"
credential = { type = "env", var = "CODEX_API_KEY" }
models = ["gpt-5.3-codex"]
default_model = "gpt-5.3-codex"
"#;

        let toml_profile: ProviderProfileConfig =
            toml::from_str(toml).expect("Codex Responses の TOML を解析できる");
        assert_eq!(
            toml_profile.api_protocol,
            ApiProtocolConfig::OpenAiCodexResponses
        );
        assert!(
            toml::to_string(&toml_profile)
                .expect("Codex Responses の TOML を直列化できる")
                .contains("api_protocol = \"openai-codex-responses\"")
        );

        let json = r#"{
            "provider_type": "openai-codex",
            "api_protocol": "openai-codex-responses",
            "base_url": "https://chatgpt.com/backend-api/codex",
            "credential": { "type": "env", "var": "CODEX_API_KEY" },
            "models": ["gpt-5.3-codex"],
            "default_model": "gpt-5.3-codex"
        }"#;

        let json_profile: ProviderProfileConfig =
            serde_json::from_str(json).expect("Codex Responses の JSON を解析できる");
        assert_eq!(
            json_profile.api_protocol,
            ApiProtocolConfig::OpenAiCodexResponses
        );
        assert_eq!(
            serde_json::to_value(&json_profile).expect("Codex Responses の JSON を直列化できる")["api_protocol"],
            "openai-codex-responses"
        );
    }
}
