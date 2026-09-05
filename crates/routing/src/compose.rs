//! 設定からprovider client群とRouterを構築する単一のcomposition rootです。
//!
//! 次のようなOpenAI互換設定を受け付けます。
//! ```toml
//! [providers.local]
//! type = "openai-compatible"
//! base_url = "http://localhost:11434/v1"
//! api_key_env = "LOCAL_API_KEY"
//! models = ["local-model"]
//! default_model = "local-model"
//! ```
//! `routing.routes`が空なら、4つのrole論理モデルと`agents`で明示された論理モデルを
//! BTreeMap順で先頭のprovider profileへ結ぶrouteを合成します。

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use event_bus::EventBus;
use providers::{ProviderAuth, ProviderClient};
use sandbox::credential::CredentialStore;

use crate::factory::{FactoryOptions, build_provider_client};
use crate::{CredentialRef, EnvLookup, ProviderProfile, Router, RoutingError};

/// composition rootへ注入する外部依存です。
pub struct ComposeDeps {
    /// Codex tokenの取得・更新に使う資格情報ストア。
    pub credential_store: Arc<dyn CredentialStore>,
    /// providerとRouterの観測イベント発行先。
    pub event_bus: Option<Arc<EventBus>>,
    /// API key用の環境変数ソース。
    pub env: Arc<dyn EnvLookup>,
    /// 組み込み・外部取得済みモデルカタログ。
    pub catalog: model::ModelCatalog,
    /// provider client factoryの上書き設定。
    pub factory: FactoryOptions,
}

/// 検証済みprofile、client、解決済みauthの組です。
pub struct ComposedProvider {
    /// 検証済みprovider profile。
    pub profile: ProviderProfile,
    /// profileに対応するprovider client。
    pub client: Arc<dyn ProviderClient>,
    /// requestごとにclientへ渡す認証情報。
    pub auth: ProviderAuth,
}

impl fmt::Debug for ComposedProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ComposedProvider")
            .field("profile", &self.profile)
            .field("client_capabilities", &self.client.capabilities())
            .finish_non_exhaustive()
    }
}

/// 構成済みprovider群と、それらを参照するRouterです。
#[derive(Debug)]
pub struct ComposedProviders {
    /// 構成済みprofileを使うRouter。
    pub router: Router,
    /// profile名をキーにした構成済みprovider群。
    pub providers: BTreeMap<String, ComposedProvider>,
}

impl ComposedProviders {
    /// providerが一件もない場合に`true`を返します。
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    /// profile名に対応する構成済みproviderを返します。
    pub fn provider(&self, name: &str) -> Option<&ComposedProvider> {
        self.providers.get(name)
    }
}

/// `agents`設定から既定routeの対象となる論理モデル名を返します。
pub fn default_logical_models(agents: &config::AgentsConfig) -> Vec<String> {
    let mut names = BTreeSet::from([
        "orchestrator".to_string(),
        "explorer".to_string(),
        "worker".to_string(),
        "reviewer".to_string(),
    ]);
    for binding in [
        &agents.orchestrator,
        &agents.explorer,
        &agents.worker,
        &agents.reviewer,
    ] {
        if let Some(logical_model) = &binding.logical_model {
            names.insert(logical_model.clone());
        }
        names.extend(
            binding
                .categories
                .values()
                .filter_map(|category| category.logical_model.clone()),
        );
    }
    names.into_iter().collect()
}

/// 設定を検証し、利用可能な全providerとRouterを一括構築します。
///
/// # Errors
/// providerがない場合、profile/client/auth/routeのいずれかを構築できない場合に
/// [`RoutingError`]を返します。一件でも失敗した場合、部分的な構成結果は返しません。
pub fn compose_providers(
    config: &config::Config,
    deps: ComposeDeps,
) -> Result<ComposedProviders, RoutingError> {
    let Some(first_profile_name) = config.providers.keys().next().cloned() else {
        return Err(RoutingError::NoProviders);
    };
    let mut catalog = deps.catalog;
    let mut profiles = Vec::with_capacity(config.providers.len());
    let mut providers = BTreeMap::new();

    for (name, profile_config) in &config.providers {
        let profile = ProviderProfile::try_from((name.as_str(), profile_config))?;
        let client = build_provider_client(
            &profile,
            Arc::clone(&deps.credential_store),
            deps.event_bus.clone(),
            &deps.factory,
        )?;
        let auth = resolve_auth(&profile, deps.env.as_ref())?;
        catalog.merge_discovered(profile.models.clone());
        profiles.push(profile.clone());
        providers.insert(
            name.clone(),
            ComposedProvider {
                profile,
                client: Arc::from(client),
                auth,
            },
        );
    }

    let effective_routes = if config.routing.routes.is_empty() {
        config::RoutingConfig {
            routes: default_logical_models(&config.agents)
                .into_iter()
                .map(|logical| {
                    (
                        logical,
                        vec![config::RouteCandidateConfig {
                            profile: first_profile_name.clone(),
                            model: None,
                        }],
                    )
                })
                .collect(),
        }
    } else {
        config.routing.clone()
    };
    let router =
        Router::new(profiles, &effective_routes, catalog)?.with_event_bus(deps.event_bus.clone());

    Ok(ComposedProviders { router, providers })
}

fn resolve_auth(
    profile: &ProviderProfile,
    env: &dyn EnvLookup,
) -> Result<ProviderAuth, RoutingError> {
    match &profile.credential {
        CredentialRef::Env { var } => {
            let value = env
                .var(var)
                .ok_or_else(|| RoutingError::MissingCredential {
                    profile: profile.name.clone(),
                    var: var.clone(),
                })?;
            if value.trim().is_empty() {
                return Err(RoutingError::EmptyCredential {
                    profile: profile.name.clone(),
                    var: var.clone(),
                });
            }
            Ok(ProviderAuth::new(value))
        }
        CredentialRef::Keyring { .. } => Ok(ProviderAuth::new("")),
    }
}
