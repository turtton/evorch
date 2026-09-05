//! 設定済み provider と runtime kernel を接続する edge composition root。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use event_bus::EventBus;
use model::{LogicalModelId, ModelCatalog};
use providers::{ChatRequest, ChatResponse, Message, ObservationContext, ToolSpec};
use routing::factory::FactoryOptions;
use routing::{ComposeDeps, ComposedProviders, RoutingError, SessionAffinity};
use sandbox::credential::CredentialStore;
use tools::ToolExecutor;

use crate::{AgentInvocationContext, AgentModel, AgentRuntime, Role, RuntimeError};

/// runtime の全外部依存を一度に渡す composition 入力。
pub struct RuntimeComposition<'a> {
    pub config: &'a config::Config,
    pub bus: Arc<EventBus>,
    pub executor: Arc<ToolExecutor>,
    pub credential_store: Arc<dyn CredentialStore>,
    pub env: Arc<dyn routing::EnvLookup>,
    pub model_source: ModelSource,
}

/// runtime が使用するモデル境界の供給元。
pub enum ModelSource {
    Configured,
    Fixed(Arc<dyn AgentModel>),
}

/// edge composition の失敗。
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum CompositionError {
    #[error("no providers configured")]
    NoProvidersConfigured,
    #[error(transparent)]
    Routing(#[from] RoutingError),
}

/// composition 時点で確定したモデル identity。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelIdentity {
    Fixed,
    Routed {
        profiles: Vec<String>,
        selected: BTreeMap<String, String>,
    },
}

/// runtime kernel と edge で確定した identity の組。
pub struct ComposedRuntime {
    pub runtime: AgentRuntime,
    pub model_identity: ModelIdentity,
}

/// 設定または固定モデルから runtime を単一経路で構築する。
///
/// # Errors
/// configured source の provider 構成が失敗した場合に返す。
pub fn compose_runtime(input: RuntimeComposition<'_>) -> Result<ComposedRuntime, CompositionError> {
    match input.model_source {
        ModelSource::Fixed(model) => Ok(ComposedRuntime {
            runtime: AgentRuntime::new(input.bus, input.executor, model),
            model_identity: ModelIdentity::Fixed,
        }),
        ModelSource::Configured => {
            let composed = routing::compose_providers(
                input.config,
                ComposeDeps {
                    credential_store: input.credential_store,
                    event_bus: Some(Arc::clone(&input.bus)),
                    env: input.env,
                    catalog: ModelCatalog::builtin(),
                    factory: FactoryOptions::default(),
                },
            )
            .map_err(|error| match error {
                RoutingError::NoProviders => CompositionError::NoProvidersConfigured,
                other => CompositionError::Routing(other),
            })?;
            let model = Arc::new(RoutedModel::new(composed, input.config.agents.clone()));
            let profiles = model.providers.keys().cloned().collect();
            let selected = routed_roles()
                .into_iter()
                .map(|role| (role_key(role).to_string(), model.selected_model(role)))
                .collect();
            Ok(ComposedRuntime {
                runtime: AgentRuntime::new(input.bus, input.executor, model),
                model_identity: ModelIdentity::Routed { profiles, selected },
            })
        }
    }
}

/// routing の解決結果を provider request へ変換する AgentModel adapter。
pub struct RoutedModel {
    router: routing::Router,
    providers: BTreeMap<String, routing::ComposedProvider>,
    affinity: Mutex<SessionAffinity>,
    agents: config::AgentsConfig,
}

impl RoutedModel {
    pub fn new(composed: ComposedProviders, agents: config::AgentsConfig) -> Self {
        Self {
            router: composed.router,
            providers: composed.providers,
            affinity: Mutex::new(SessionAffinity::default()),
            agents,
        }
    }

    fn resolve(
        &self,
        session_id: &str,
        logical: &LogicalModelId,
    ) -> Result<routing::ResolvedRoute, RuntimeError> {
        self.router
            .resolve(
                &mut self
                    .affinity
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                session_id,
                logical,
            )
            .map_err(model_error)
    }
}

#[async_trait]
impl AgentModel for RoutedModel {
    async fn complete(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        let binding = self
            .agents
            .binding_for(role_key(role), None)
            .map_err(model_error)?;
        let route = self.resolve(
            &invocation.run_id,
            &LogicalModelId::from(binding.logical_model),
        )?;
        let provider = self
            .providers
            .get(&route.profile)
            .ok_or_else(|| RuntimeError::Model {
                reason: "resolved provider profile is unavailable".to_string(),
            })?;
        let request = ChatRequest {
            model: route.model_id,
            messages: messages.to_vec(),
            tools: tools.to_vec(),
            temperature: binding.generation.temperature,
            max_tokens: binding.generation.max_tokens.map(u64::from),
            observation: Some(ObservationContext {
                run_id: invocation.run_id.clone(),
            }),
        };
        provider
            .client
            .send(&provider.auth, &request)
            .await
            .map_err(|_| RuntimeError::Model {
                reason: "provider request failed".to_string(),
            })
    }

    fn selected_model(&self, role: Role) -> String {
        let Ok(binding) = self.agents.binding_for(role_key(role), None) else {
            return format!("unresolved:{}", role_key(role));
        };
        let logical = LogicalModelId::from(binding.logical_model);
        self.resolve("runtime-selected-model", &logical)
            .map(|route| format!("{}/{}", route.profile, route.model_id))
            .unwrap_or_else(|_| format!("unresolved:{}", logical.as_str()))
    }
}

fn model_error(error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::Model {
        reason: error.to_string(),
    }
}

const fn routed_roles() -> [Role; 4] {
    [
        Role::Orchestrator,
        Role::Explorer,
        Role::Worker,
        Role::Reviewer,
    ]
}

const fn role_key(role: Role) -> &'static str {
    match role {
        Role::Orchestrator => "orchestrator",
        Role::Explorer => "explorer",
        Role::Worker => "worker",
        Role::Reviewer => "reviewer",
        Role::Librarian => "librarian",
    }
}

#[cfg(test)]
mod tests;
