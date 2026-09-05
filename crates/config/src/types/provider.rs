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
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(transform = add_sugar_properties)]
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

// sugar 形式 (type エイリアスと api_key_env) を正規形へ畳み込む必要があるため、
// CredentialRefDe と同じミラー構造体パターンで deserialization を行う。
// deny_unknown_fields はミラー側で維持される。
#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ProviderProfileDe {
    #[serde(alias = "type")]
    provider_type: ProviderTypeConfig,
    api_protocol: Option<ApiProtocolConfig>,
    base_url: String,
    credential: Option<CredentialRefConfig>,
    api_key_env: Option<String>,
    models: Vec<String>,
    default_model: String,
}

// 省略フィールドは公開構造体の既定値で補完する (コンテナ serde(default) の契約)。
// api_protocol と credential の既定化は TryFrom 側で sugar 規則を考慮して行う。
impl Default for ProviderProfileDe {
    fn default() -> Self {
        Self {
            provider_type: ProviderTypeConfig::default(),
            api_protocol: None,
            base_url: "https://api.anthropic.com".to_string(),
            credential: None,
            api_key_env: None,
            models: vec!["claude-sonnet-4-5".to_string()],
            default_model: "claude-sonnet-4-5".to_string(),
        }
    }
}

impl TryFrom<ProviderProfileDe> for ProviderProfileConfig {
    type Error = String;

    fn try_from(value: ProviderProfileDe) -> Result<Self, Self::Error> {
        if value.credential.is_some() && value.api_key_env.is_some() {
            return Err(
                "api_key_env and credential are mutually exclusive; use one or the other"
                    .to_string(),
            );
        }
        if value
            .api_key_env
            .as_ref()
            .is_some_and(|var| var.trim().is_empty())
        {
            return Err("api_key_env must not be empty".to_string());
        }
        let credential = match value.api_key_env {
            Some(var) => CredentialRefConfig::Env { var },
            None => value.credential.unwrap_or_default(),
        };
        // api_protocol が省略された場合のみ、openai-compatible の既定プロトコル
        // (openai-completions) を適用する。明示指定は常に優先される。
        let api_protocol = value.api_protocol.unwrap_or(match value.provider_type {
            ProviderTypeConfig::OpenAiCompatible => ApiProtocolConfig::OpenAiCompletions,
            _ => ApiProtocolConfig::default(),
        });
        Ok(Self {
            provider_type: value.provider_type,
            api_protocol,
            base_url: value.base_url,
            credential,
            models: value.models,
            default_model: value.default_model,
        })
    }
}

impl<'de> Deserialize<'de> for ProviderProfileConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        ProviderProfileConfig::try_from(ProviderProfileDe::deserialize(deserializer)?)
            .map_err(serde::de::Error::custom)
    }
}

// 公開フィールドに加えて sugar 形式の type エイリアスと api_key_env を
// optional property として schema に載せる (deny_unknown_fields 対応のため
// additionalProperties は false のまま)。
fn add_sugar_properties(schema: &mut schemars::Schema) {
    let object = schema.ensure_object();
    let properties = object
        .entry("properties")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    let Some(properties) = properties.as_object_mut() else {
        return;
    };
    properties.insert(
        "type".to_string(),
        serde_json::json!({
            "description": "`provider_type` のエイリアス。",
            "$ref": "#/$defs/ProviderTypeConfig"
        }),
    );
    properties.insert(
        "api_key_env".to_string(),
        serde_json::json!({
            "description": "`credential = { type = \"env\", var = \"...\" }` の糖衣構文。`credential` とは併用できない。",
            "type": "string",
            "minLength": 1
        }),
    );
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

    // Given: openai-compatible の sugar 形式 (type エイリアス + api_key_env) / When: TOML をパースする
    // Then: type エイリアスが解決され、api_key_env が env credential へ正規化され、
    //       api_protocol は openai-completions へ自動既定化される
    #[test]
    fn provider_profile_openai_compatible_sugar_parses() {
        let doc = r#"
type = "openai-compatible"
base_url = "http://127.0.0.1:8080/v1"
api_key_env = "LOCAL_API_KEY"
models = ["local-model"]
default_model = "local-model"
"#;

        let profile: ProviderProfileConfig =
            toml::from_str(doc).expect("openai-compatible の sugar 形式を解析できる");

        assert_eq!(profile.provider_type, ProviderTypeConfig::OpenAiCompatible);
        assert_eq!(
            profile.credential,
            CredentialRefConfig::Env {
                var: "LOCAL_API_KEY".to_string()
            }
        );
        assert_eq!(profile.api_protocol, ApiProtocolConfig::OpenAiCompletions);
        assert_eq!(profile.base_url, "http://127.0.0.1:8080/v1");
        assert_eq!(profile.models, ["local-model"]);
        assert_eq!(profile.default_model, "local-model");
    }

    // Given: 正式キー provider_type = "openai-compatible" で api_protocol 省略 / When: パースする
    // Then: openai-completions へ自動既定化される (type は provider_type のエイリアスのため)
    #[test]
    fn provider_profile_openai_compatible_canonical_key_auto_protocol_default() {
        let doc = r#"
provider_type = "openai-compatible"
base_url = "http://127.0.0.1:8080/v1"
api_key_env = "LOCAL_API_KEY"
models = ["local-model"]
default_model = "local-model"
"#;

        let profile: ProviderProfileConfig =
            toml::from_str(doc).expect("正式キーの openai-compatible を解析できる");

        assert_eq!(profile.api_protocol, ApiProtocolConfig::OpenAiCompletions);
    }

    // Given: openai-compatible 以外で api_protocol 省略 / When: パースする
    // Then: 従来どおり anthropic-messages が既定になる
    #[test]
    fn provider_profile_non_compatible_keeps_default_protocol() {
        let doc = r#"
provider_type = "openrouter"
base_url = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_API_KEY"
models = ["model-a"]
default_model = "model-a"
"#;

        let profile: ProviderProfileConfig =
            toml::from_str(doc).expect("openrouter の sugar 形式を解析できる");

        assert_eq!(profile.api_protocol, ApiProtocolConfig::AnthropicMessages);
        assert_eq!(
            profile.credential,
            CredentialRefConfig::Env {
                var: "OPENROUTER_API_KEY".to_string()
            }
        );
    }

    // Given: openai-compatible で明示的な api_protocol / When: パースする
    // Then: 自動既定化より明示指定が優先される
    #[test]
    fn provider_profile_explicit_api_protocol_wins_over_sugar_default() {
        let doc = r#"
type = "openai-compatible"
api_protocol = "openai-responses"
base_url = "http://127.0.0.1:8080/v1"
api_key_env = "LOCAL_API_KEY"
models = ["local-model"]
default_model = "local-model"
"#;

        let profile: ProviderProfileConfig =
            toml::from_str(doc).expect("明示 protocol 付き sugar 形式を解析できる");

        assert_eq!(profile.api_protocol, ApiProtocolConfig::OpenAiResponses);
    }

    // Given: api_key_env と credential の併用 / When: パースする
    // Then: エラーとして拒否される
    #[test]
    fn provider_profile_api_key_env_and_credential_mutually_exclusive() {
        let doc = r#"
type = "openai-compatible"
base_url = "http://127.0.0.1:8080/v1"
api_key_env = "LOCAL_API_KEY"
credential = { type = "env", var = "OTHER_KEY" }
models = ["local-model"]
default_model = "local-model"
"#;

        let result = toml::from_str::<ProviderProfileConfig>(doc);

        let err = result.expect_err("api_key_env と credential の併用は拒否される");
        assert!(
            err.to_string().contains("mutually exclusive"),
            "併用エラーであることを示す: {err}"
        );
    }

    // Given: 空文字列または空白のみの api_key_env / When: パースする
    // Then: エラーとして拒否される
    #[test]
    fn provider_profile_api_key_env_empty_rejected() {
        for var in ["", "   "] {
            let doc = format!(
                r#"
type = "openai-compatible"
base_url = "http://127.0.0.1:8080/v1"
api_key_env = "{var}"
models = ["local-model"]
default_model = "local-model"
"#
            );

            let result = toml::from_str::<ProviderProfileConfig>(&doc);

            assert!(
                result.is_err(),
                "空の api_key_env ({var:?}) は拒否される: {result:?}"
            );
        }
    }

    // Given: sugar 形式でパースしたプロファイル / When: TOML へ直列化する
    // Then: 正規形 (provider_type / credential) のみが出力され、sugar は現れず、
    //       正規形として再パースできる
    #[test]
    fn provider_profile_sugar_serializes_to_canonical_form() {
        let doc = r#"
type = "openai-compatible"
base_url = "http://127.0.0.1:8080/v1"
api_key_env = "LOCAL_API_KEY"
models = ["local-model"]
default_model = "local-model"
"#;
        let profile: ProviderProfileConfig = toml::from_str(doc).expect("sugar 形式を解析できる");

        let serialized = toml::to_string(&profile).expect("正規形へ直列化できる");
        assert!(
            serialized.contains("provider_type = \"openai-compatible\""),
            "provider_type を出力する: {serialized}"
        );
        assert!(
            serialized.contains("type = \"env\""),
            "credential の type タグを出力する: {serialized}"
        );
        assert!(
            !serialized.contains("api_key_env"),
            "sugar を出力しない: {serialized}"
        );

        let reparsed: ProviderProfileConfig =
            toml::from_str(&serialized).expect("正規形として再パースできる");
        assert_eq!(reparsed, profile);
    }

    // Given: sugar 形式に未知キーを混在させる / When: パースする
    // Then: 未知キーは deny_unknown_fields により拒否される
    #[test]
    fn provider_profile_unknown_field_rejected_alongside_sugar() {
        let doc = r#"
type = "openai-compatible"
api_key_env = "LOCAL_API_KEY"
base_url = "http://127.0.0.1:8080/v1"
models = ["local-model"]
default_model = "local-model"
typo_field = true
"#;

        let result = toml::from_str::<ProviderProfileConfig>(doc);

        assert!(
            result.is_err(),
            "sugar 形式でも未知フィールドは拒否される: {result:?}"
        );
    }

    // Given: 不正な type エイリアス値 / When: パースする
    // Then: ProviderTypeConfig の variant でないため拒否される
    #[test]
    fn provider_profile_invalid_type_alias_rejected() {
        let result = toml::from_str::<ProviderProfileConfig>(
            "type = \"not-a-provider\"\nbase_url = \"https://x\"\n",
        );

        assert!(result.is_err(), "不正な type 値は拒否される: {result:?}");
    }

    // Given: ProviderProfileConfig の生成 JSON Schema / When: properties を確認する
    // Then: sugar 形式の type エイリアスと api_key_env が optional property として含まれる
    #[test]
    fn provider_profile_schema_includes_sugar_properties() {
        let schema = schemars::schema_for!(ProviderProfileConfig);
        let json = serde_json::to_value(&schema).expect("schema を JSON 化できる");

        let properties = json["properties"]
            .as_object()
            .expect("schema は object properties を持つ");
        assert!(
            properties.contains_key("type"),
            "type エイリアスを schema に含める"
        );
        assert!(
            properties.contains_key("api_key_env"),
            "api_key_env を schema に含める"
        );
        assert!(
            !json["required"]
                .as_array()
                .map(|required| required
                    .iter()
                    .any(|name| name == "type" || name == "api_key_env"))
                .unwrap_or(false),
            "sugar property は必須にしない"
        );
    }
}
