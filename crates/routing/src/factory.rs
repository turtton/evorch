//! [`ProviderProfile`] から provider client を構築するファクトリを提供します。

use std::sync::Arc;

use event_bus::EventBus;
use providers::ProviderClient;
use providers::error::ProviderError;
use providers::provider::codex::tokens::{CodexTokenStore, TokenBundle};
use providers::provider::codex::{CodexClient, CodexConfig};
use sandbox::credential::{CredentialStore, Secret};

use crate::{CredentialRef, ProviderProfile, RoutingError};

/// codex の OAuth refresh endpoint の既定ベース URL。
pub const DEFAULT_AUTH_BASE_URL: &str = "https://auth.openai.com";
/// codex backend の既定ベース URL。
pub const DEFAULT_CODEX_BASE_URL: &str = "https://chatgpt.com";

/// [`sandbox::credential::CredentialStore`] を codex の [`CodexTokenStore`]
/// 契約へ適合させるアダプタ。
///
/// トークン一式は `key` で指定した認証情報キーに単一の JSON オブジェクト
/// (`access_token` / `refresh_token` / `id_token`) として保存する。
pub struct CredentialStoreTokenStore {
    store: Arc<dyn CredentialStore>,
    key: String,
}

impl CredentialStoreTokenStore {
    /// 認証情報ストアとキーからアダプタを生成します。
    pub fn new(store: Arc<dyn CredentialStore>, key: String) -> Self {
        Self { store, key }
    }
}

impl CodexTokenStore for CredentialStoreTokenStore {
    fn load(&self) -> Result<Option<TokenBundle>, ProviderError> {
        let Some(secret) = self
            .store
            .get(&self.key)
            .map_err(credential_store_failure)?
        else {
            return Ok(None);
        };
        serde_json::from_str(secret.expose())
            .map(Some)
            .map_err(|error| ProviderError::InvalidJson {
                detail: format!("保存済みトークン一式の解析に失敗しました: {error}"),
            })
    }

    fn save(&self, bundle: &TokenBundle) -> Result<(), ProviderError> {
        let json = serde_json::to_string(bundle).map_err(|error| ProviderError::InvalidJson {
            detail: format!("トークン一式の保存用 JSON への変換に失敗しました: {error}"),
        })?;
        self.store
            .set(&self.key, &Secret::from(json))
            .map_err(credential_store_failure)
    }
}

/// [`FactoryOptions`] は codex client 構築時の上書き設定です。
#[derive(Debug, Clone, Default)]
pub struct FactoryOptions {
    /// OAuth 認証先ベース URL の上書き。`None` なら [`DEFAULT_AUTH_BASE_URL`]
    /// を使用する。
    pub auth_base_url_override: Option<String>,
}

/// プロファイルから codex subscription client を構築します。
///
/// codex 以外の [`model::ProviderType`] は未対応で、
/// [`RoutingError::UnsupportedProviderType`] を返します。codex の場合は
/// protocol が `openai-codex-responses` かつ認証参照が
/// [`CredentialRef::Keyring`] であることを要求し、違反は
/// [`RoutingError::InvalidProfile`] で通知します。
///
/// キーリングの参照先は `credential.account` をキーとし、service フィールドは
/// 現状の [`sandbox::credential::CredentialStore`] 実装では装飾的な値です。
///
/// # Errors
/// 種別・protocol・認証参照の検証失敗、または HTTP client の構築失敗時に
/// [`RoutingError`] を返します。
pub fn build_provider_client(
    profile: &ProviderProfile,
    store: Arc<dyn CredentialStore>,
    event_bus: Option<Arc<EventBus>>,
    options: &FactoryOptions,
) -> Result<Box<dyn ProviderClient>, RoutingError> {
    match profile.provider_type {
        model::ProviderType::OpenAiCodex => {}
        other => {
            return Err(RoutingError::UnsupportedProviderType {
                provider_type: provider_type_label(other).to_string(),
            });
        }
    }
    if profile.api_protocol != model::ApiProtocol::OpenAiCodexResponses {
        return Err(RoutingError::InvalidProfile {
            reason: format!(
                "provider type `openai-codex` は api protocol `openai-codex-responses` のみを\
                 サポートします (actual: {})。api_protocol を `openai-codex-responses` に\
                 変更してください",
                protocol_label(profile.api_protocol)
            ),
        });
    }
    let account = match &profile.credential {
        CredentialRef::Keyring { account, .. } => account.clone(),
        CredentialRef::Env { .. } => {
            return Err(RoutingError::InvalidProfile {
                reason: "provider type `openai-codex` は keyring 認証情報参照のみをサポート\
                 します。環境変数参照ではなく keyring 参照 \
                 (service = \"evorch\", account = \"...\") に変更してください"
                    .to_string(),
            });
        }
    };

    let config = CodexConfig {
        base_url: resolve_base_url(profile),
        auth_base_url: options
            .auth_base_url_override
            .clone()
            .unwrap_or_else(|| DEFAULT_AUTH_BASE_URL.to_string()),
        event_bus,
        ..CodexConfig::default()
    };
    let token_store = Arc::new(CredentialStoreTokenStore::new(store, account));
    let client = CodexClient::with_config(config, token_store).map_err(|error| {
        RoutingError::InvalidProfile {
            reason: format!("codex client の構築に失敗しました: {error}"),
        }
    })?;
    Ok(Box::new(client))
}

/// プロファイルの `base_url` を解決する。空なら codex 既定へフォールバックする。
fn resolve_base_url(profile: &ProviderProfile) -> String {
    if profile.base_url.is_empty() {
        DEFAULT_CODEX_BASE_URL.to_string()
    } else {
        profile.base_url.clone()
    }
}

/// [`CredentialError`] を provider 契約のエラーへ写像する。
fn credential_store_failure(error: sandbox::error::CredentialError) -> ProviderError {
    ProviderError::Request(format!("credential store 操作に失敗しました: {error}"))
}

/// プロバイダ種別の設定上の識別子を返す。
const fn provider_type_label(provider_type: model::ProviderType) -> &'static str {
    match provider_type {
        model::ProviderType::Anthropic => "anthropic",
        model::ProviderType::AnthropicSubscription => "anthropic-subscription",
        model::ProviderType::OpenAi => "openai",
        model::ProviderType::OpenAiCodex => "openai-codex",
        model::ProviderType::GithubCopilot => "github-copilot",
        model::ProviderType::Openrouter => "openrouter",
        model::ProviderType::OpenAiCompatible => "openai-compatible",
    }
}

/// API プロトコルの設定上の識別子を返す。
const fn protocol_label(protocol: model::ApiProtocol) -> &'static str {
    match protocol {
        model::ApiProtocol::AnthropicMessages => "anthropic-messages",
        model::ApiProtocol::OpenAiResponses => "openai-responses",
        model::ApiProtocol::OpenAiCompletions => "openai-completions",
        model::ApiProtocol::OpenAiCodexResponses => "openai-codex-responses",
    }
}
