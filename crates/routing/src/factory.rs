//! [`ProviderProfile`] から provider client を構築するファクトリを提供します。

use std::sync::Arc;
use std::time::Duration;

use event_bus::EventBus;
use providers::ProviderClient;
use providers::error::ProviderError;
use providers::provider::anthropic::{AnthropicClient, AnthropicConfig};
use providers::provider::claude::{ClaudeClient, ClaudeTokenBundle, ClaudeTokenStore};
use providers::provider::codex::tokens::{CodexTokenStore, TokenBundle};
use providers::provider::codex::{CodexClient, CodexConfig};
use providers::provider::cursor::{
    CursorClient, CursorConfig, CursorTokenBundle, CursorTokenStore,
};
use providers::provider::openai_compatible::OpenAiCompatibleClient;
use sandbox::credential::{CredentialStore, Secret};

use crate::{CredentialRef, ProviderProfile, RoutingError};

/// codex の OAuth refresh endpoint の既定ベース URL。
pub const DEFAULT_AUTH_BASE_URL: &str = "https://auth.openai.com";
/// codex backend の既定ベース URL。
pub const DEFAULT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
/// provider request の既定タイムアウト。
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

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

/// Claude OAuth bundles live in the credential store, never in config or GUI state.
pub struct CredentialStoreClaudeTokenStore {
    store: Arc<dyn CredentialStore>,
    key: String,
}

impl CredentialStoreClaudeTokenStore {
    pub fn new(store: Arc<dyn CredentialStore>, key: String) -> Self {
        Self { store, key }
    }
}

impl ClaudeTokenStore for CredentialStoreClaudeTokenStore {
    fn refresh_lock(&self) -> Arc<tokio::sync::Mutex<()>> {
        subscription_refresh_lock("claude", &self.key)
    }
    fn load(&self) -> Result<Option<ClaudeTokenBundle>, ProviderError> {
        let Some(secret) = self
            .store
            .get(&self.key)
            .map_err(credential_store_failure)?
        else {
            return Ok(None);
        };
        serde_json::from_str(secret.expose())
            .map(Some)
            .map_err(|_| ProviderError::InvalidJson {
                detail: "Stored Claude credentials are invalid; sign in again".into(),
            })
    }

    fn save(&self, bundle: &ClaudeTokenBundle) -> Result<(), ProviderError> {
        let json = serde_json::to_string(bundle).map_err(|_| ProviderError::InvalidJson {
            detail: "Could not encode Claude credentials".into(),
        })?;
        self.store
            .set(&self.key, &Secret::from(json))
            .map_err(credential_store_failure)
    }
}

/// Native Cursor OAuth storage adapter.
pub struct CredentialStoreCursorTokenStore {
    store: Arc<dyn CredentialStore>,
    key: String,
}

impl CredentialStoreCursorTokenStore {
    pub fn new(store: Arc<dyn CredentialStore>, key: String) -> Self {
        Self { store, key }
    }
}

impl CursorTokenStore for CredentialStoreCursorTokenStore {
    fn refresh_lock(&self) -> Arc<tokio::sync::Mutex<()>> {
        subscription_refresh_lock("cursor", &self.key)
    }
    fn load(&self) -> Result<Option<CursorTokenBundle>, ProviderError> {
        let Some(secret) = self
            .store
            .get(&self.key)
            .map_err(credential_store_failure)?
        else {
            return Ok(None);
        };
        serde_json::from_str(secret.expose())
            .map(Some)
            .map_err(|_| ProviderError::InvalidJson {
                detail: "Stored Cursor credentials are invalid; sign in again".into(),
            })
    }
    fn save(&self, bundle: &CursorTokenBundle) -> Result<(), ProviderError> {
        let json = serde_json::to_string(bundle).map_err(|_| ProviderError::InvalidJson {
            detail: "Could not encode Cursor credentials".into(),
        })?;
        self.store
            .set(&self.key, &Secret::from(json))
            .map_err(credential_store_failure)
    }
}

/// All clients for one credential account serialize refresh and store rotation.
pub fn subscription_refresh_lock(
    provider: &'static str,
    account: &str,
) -> Arc<tokio::sync::Mutex<()>> {
    use std::collections::BTreeMap;
    use std::sync::{Mutex, OnceLock, Weak};
    type Locks = BTreeMap<(&'static str, String), Weak<tokio::sync::Mutex<()>>>;
    static LOCKS: OnceLock<Mutex<Locks>> = OnceLock::new();
    let mut locks = LOCKS
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    locks.retain(|_, lock| lock.strong_count() > 0);
    let key = (provider, account.to_owned());
    if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    locks.insert(key, Arc::downgrade(&lock));
    lock
}

/// [`FactoryOptions`] は provider client 構築時の上書き設定です。
#[derive(Debug, Clone, Default)]
pub struct FactoryOptions {
    /// OAuth 認証先ベース URL の上書き。`None` なら [`DEFAULT_AUTH_BASE_URL`]
    /// を使用する。
    pub auth_base_url_override: Option<String>,
    /// provider request timeout の上書き。`None` なら60秒。
    pub request_timeout: Option<Duration>,
    /// Codex client version の上書き。`None` なら共有 resolver で一度だけ解決する。
    pub codex_client_version: Option<providers::CodexClientVersion>,
}

/// プロファイルから対応する provider client を構築します。
///
/// Codex、Claude OAuth、Cursor はネイティブ protocol と
/// [`CredentialRef::Keyring`] を要求します。Claude API は Anthropic Messages、
/// OpenAI 互換・Kimi は OpenAI Completions を使用します。
/// 未実装の種別は [`RoutingError::UnsupportedProviderType`] を返します。
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
        model::ProviderType::Cursor => build_cursor(profile, store, event_bus, options),
        model::ProviderType::Anthropic | model::ProviderType::AnthropicSubscription => {
            build_claude(profile, store, event_bus, options)
        }
        model::ProviderType::OpenAiCodex => build_codex(profile, store, event_bus, options),
        model::ProviderType::OpenAiCompatible | model::ProviderType::KimiSubscription => {
            build_openai_compatible(profile, event_bus, options)
        }
        other @ (model::ProviderType::OpenAi
        | model::ProviderType::GithubCopilot
        | model::ProviderType::Openrouter) => Err(RoutingError::UnsupportedProviderType {
            provider_type: provider_type_label(other).to_string(),
        }),
    }
}

fn build_cursor(
    profile: &ProviderProfile,
    store: Arc<dyn CredentialStore>,
    event_bus: Option<Arc<EventBus>>,
    options: &FactoryOptions,
) -> Result<Box<dyn ProviderClient>, RoutingError> {
    if profile.api_protocol != model::ApiProtocol::CursorAgent {
        return Err(RoutingError::InvalidProfile {
            reason: "Cursor requires cursor-agent".into(),
        });
    }
    let CredentialRef::Keyring { account, .. } = &profile.credential else {
        return Err(RoutingError::InvalidProfile {
            reason: "Cursor OAuth requires a keyring credential reference".into(),
        });
    };
    let config = CursorConfig {
        base_url: if profile.base_url.trim().is_empty() {
            "https://api2.cursor.sh".into()
        } else {
            profile.base_url.clone()
        },
        event_bus,
    };
    let fail = |error| RoutingError::InvalidProfile {
        reason: format!("Could not build Cursor client: {error}"),
    };
    let mut client = CursorClient::new(
        config,
        Arc::new(CredentialStoreCursorTokenStore::new(store, account.clone())),
    )
    .map_err(fail)?;
    if let Some(base) = &options.auth_base_url_override {
        let oauth = providers::provider::cursor::CursorOAuthConfig {
            token_url: format!("{}/oauth/token", base.trim_end_matches('/')),
            ..Default::default()
        };
        client = client.with_oauth_config(oauth).map_err(fail)?;
    }
    Ok(Box::new(client.with_profile(&profile.name)))
}

fn build_claude(
    profile: &ProviderProfile,
    store: Arc<dyn CredentialStore>,
    event_bus: Option<Arc<EventBus>>,
    options: &FactoryOptions,
) -> Result<Box<dyn ProviderClient>, RoutingError> {
    if profile.api_protocol != model::ApiProtocol::AnthropicMessages {
        return Err(RoutingError::InvalidProfile {
            reason: "Claude requires anthropic-messages".into(),
        });
    }
    let base = profile.base_url.trim_end_matches('/');
    let base_url = if base.is_empty() {
        "https://api.anthropic.com/v1".into()
    } else if base.ends_with("/v1") {
        base.to_owned()
    } else {
        format!("{base}/v1")
    };
    let config = AnthropicConfig {
        base_url,
        timeout: options.request_timeout.unwrap_or(DEFAULT_REQUEST_TIMEOUT),
        event_bus,
    };
    let fail = |error| RoutingError::InvalidProfile {
        reason: format!("Could not build Claude client: {error}"),
    };
    if profile.provider_type == model::ProviderType::Anthropic {
        return AnthropicClient::new(config)
            .map(|client| Box::new(client.with_profile(&profile.name)) as Box<dyn ProviderClient>)
            .map_err(fail);
    }
    let CredentialRef::Keyring { account, .. } = &profile.credential else {
        return Err(RoutingError::InvalidProfile {
            reason: "Claude OAuth requires a keyring credential reference".into(),
        });
    };
    let tokens = Arc::new(CredentialStoreClaudeTokenStore::new(store, account.clone()));
    let mut client = ClaudeClient::new(config, tokens).map_err(fail)?;
    if let Some(base) = &options.auth_base_url_override {
        let oauth = providers::provider::claude::ClaudeOAuthConfig {
            token_url: format!("{}/v1/oauth/token", base.trim_end_matches('/')),
            ..Default::default()
        };
        client = client.with_oauth_config(oauth).map_err(fail)?;
    }
    Ok(Box::new(client.with_profile(&profile.name)))
}

fn build_codex(
    profile: &ProviderProfile,
    store: Arc<dyn CredentialStore>,
    event_bus: Option<Arc<EventBus>>,
    options: &FactoryOptions,
) -> Result<Box<dyn ProviderClient>, RoutingError> {
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
        client_version: options.codex_client_version.clone().unwrap_or_default(),
        event_bus,
        ..CodexConfig::default()
    };
    let token_store = Arc::new(CredentialStoreTokenStore::new(store, account));
    let client = CodexClient::with_config(config, token_store).map_err(|error| {
        RoutingError::InvalidProfile {
            reason: format!("codex client の構築に失敗しました: {error}"),
        }
    })?;
    Ok(Box::new(client.with_profile(&profile.name)))
}

fn build_openai_compatible(
    profile: &ProviderProfile,
    event_bus: Option<Arc<EventBus>>,
    options: &FactoryOptions,
) -> Result<Box<dyn ProviderClient>, RoutingError> {
    if profile.api_protocol != model::ApiProtocol::OpenAiCompletions {
        return Err(RoutingError::InvalidProfile {
            reason: format!(
                "provider type `{}` は api protocol `openai-completions` のみをサポートします (actual: {})",
                provider_type_label(profile.provider_type),
                protocol_label(profile.api_protocol)
            ),
        });
    }
    let timeout = options.request_timeout.unwrap_or(DEFAULT_REQUEST_TIMEOUT);
    let client = OpenAiCompatibleClient::new(
        &profile.base_url,
        provider_type_label(profile.provider_type),
        timeout,
        event_bus,
    )
    .map_err(|error| RoutingError::InvalidProfile {
        reason: format!("openai-compatible client の構築に失敗しました: {error}"),
    })?
    .with_profile(&profile.name);
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
        model::ProviderType::KimiSubscription => "kimi-subscription",
        model::ProviderType::Cursor => "cursor",
    }
}

/// API プロトコルの設定上の識別子を返す。
const fn protocol_label(protocol: model::ApiProtocol) -> &'static str {
    match protocol {
        model::ApiProtocol::AnthropicMessages => "anthropic-messages",
        model::ApiProtocol::OpenAiResponses => "openai-responses",
        model::ApiProtocol::OpenAiCompletions => "openai-completions",
        model::ApiProtocol::OpenAiCodexResponses => "openai-codex-responses",
        model::ApiProtocol::CursorAgent => "cursor-agent",
    }
}
