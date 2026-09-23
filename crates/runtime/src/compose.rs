//! 設定済み provider と runtime kernel を接続する edge composition root。
// allow: SIZE_OK — T2 restricts runtime production edits to model.rs/compose.rs;
// retain the existing composition root and shared route/error path in this file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use event_bus::EventBus;
use model::{Capability, CapabilitySupport, LogicalModelId, ModelCatalog};
use providers::{ChatRequest, ChatResponse, Message, ObservationContext, ToolSpec};
use routing::factory::FactoryOptions;
use routing::{ComposeDeps, ComposedProviders, RoutingError, SessionAffinity};
use sandbox::credential::CredentialStore;
use tools::ToolExecutor;

use crate::workspace::{Project, WorktreeManager};
use crate::{AgentInvocationContext, AgentModel, AgentRuntime, Role, RuntimeError};

/// composition root に production workspace context を渡す seam。
pub struct WorkspaceSeam {
    project: Project,
    factory: Arc<dyn crate::SandboxFactory>,
}

impl WorkspaceSeam {
    /// production project を検証して workspace seam を生成する。
    ///
    /// # Errors
    /// project root が有効な git repository でない場合に [`RuntimeError::Workspace`] を返す。
    pub fn production(project_root: PathBuf) -> Result<Self, RuntimeError> {
        Self::with_factory(project_root, Arc::new(crate::network::BwrapFactory))
    }

    /// sandbox factory を明示した workspace seam を生成する。
    ///
    /// bwrap 実行環境を持たない CI でも isolated workspace の結線を検証できるように
    /// するテスト seam。production 経路は [`WorkspaceSeam::production`] を使うこと。
    ///
    /// # Errors
    /// project root が有効な git repository でない場合に [`RuntimeError::Workspace`] を返す。
    pub fn with_factory(
        project_root: PathBuf,
        factory: Arc<dyn crate::SandboxFactory>,
    ) -> Result<Self, RuntimeError> {
        let project = Project::new(project_root).map_err(|error| RuntimeError::Workspace {
            detail: error.to_string(),
        })?;
        Ok(Self { project, factory })
    }

    /// 検証済み repository root を返す。
    pub fn repo_root(&self) -> &Path {
        self.project.repo_root()
    }

    pub(crate) fn into_manager_and_factory(
        self,
    ) -> (WorktreeManager, Arc<dyn crate::SandboxFactory>) {
        (WorktreeManager::new(self.project), self.factory)
    }
}

/// runtime の全外部依存を一度に渡す composition 入力。
pub struct RuntimeComposition<'a> {
    pub config: &'a config::Config,
    pub bus: Arc<EventBus>,
    pub executor: Arc<ToolExecutor>,
    pub credential_store: Arc<dyn CredentialStore>,
    pub env: Arc<dyn routing::EnvLookup>,
    pub model_source: ModelSource,
    pub workspace: Option<WorkspaceSeam>,
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
    let composed = match input.model_source {
        ModelSource::Fixed(model) => ComposedRuntime {
            runtime: compose_agent_runtime(input.bus, input.executor, model, input.workspace),
            model_identity: ModelIdentity::Fixed,
        },
        ModelSource::Configured => {
            let model = compose_routed_model(
                input.config,
                ComposeDeps {
                    credential_store: Arc::clone(&input.credential_store),
                    event_bus: Some(Arc::clone(&input.bus)),
                    env: input.env,
                    catalog: ModelCatalog::new(),
                    factory: FactoryOptions::default(),
                },
            )?;
            let profiles = model.providers.keys().cloned().collect();
            let selected = routed_roles()
                .into_iter()
                .map(|role| (role_key(role).to_string(), model.selected_model(role, None)))
                .collect();
            ComposedRuntime {
                runtime: compose_agent_runtime(input.bus, input.executor, model, input.workspace),
                model_identity: ModelIdentity::Routed { profiles, selected },
            }
        }
    };
    let runtime = composed
        .runtime
        .with_model_resolution(input.config, Some(input.credential_store));
    runtime.configure_shell_escalation(
        &runtime
            .shared
            .executor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    Ok(ComposedRuntime {
        runtime,
        model_identity: composed.model_identity,
    })
}

fn compose_agent_runtime(
    bus: Arc<EventBus>,
    executor: Arc<ToolExecutor>,
    model: Arc<dyn AgentModel>,
    workspace: Option<WorkspaceSeam>,
) -> AgentRuntime {
    match workspace {
        Some(seam) => {
            let (manager, factory) = seam.into_manager_and_factory();
            AgentRuntime::with_workspace_context(bus, executor, model, manager, factory)
        }
        None => AgentRuntime::new(bus, executor, model),
    }
}

/// routing の解決結果を provider request へ変換する AgentModel adapter。
pub struct RoutedModel {
    router: routing::Router,
    tool_router: routing::Router,
    providers: BTreeMap<String, routing::ComposedProvider>,
    affinity: Mutex<SessionAffinity>,
    agents: config::AgentsConfig,
    admission_routes: Vec<routing::ResolvedRoute>,
    verification:
        tokio::sync::OnceCell<BTreeMap<String, Result<Vec<String>, providers::ProviderError>>>,
    codex_version_resolver: std::sync::Arc<providers::CodexCatalogVersionResolver>,
    event_bus: Option<Arc<EventBus>>,
    credential_store: Option<Arc<dyn CredentialStore>>,
}

impl RoutedModel {
    /// Returns credential-free profiles in name order.
    pub fn available_profiles(&self) -> Vec<ProfileSummary> {
        self.providers
            .iter()
            .map(|(name, provider)| ProfileSummary {
                name: name.clone(),
                provider_type: provider.profile.provider_type,
                models: provider.profile.models.clone(),
                default_model: Some(provider.profile.default_model.clone()),
            })
            .collect()
    }

    pub fn new(composed: ComposedProviders, agents: config::AgentsConfig) -> Self {
        Self {
            tool_router: composed
                .router
                .clone()
                .requiring_capability(Capability::ToolCalling),
            router: composed.router,
            providers: composed.providers,
            affinity: Mutex::new(SessionAffinity::default()),
            agents,
            admission_routes: Vec::new(),
            verification: tokio::sync::OnceCell::new(),
            codex_version_resolver: providers::CodexCatalogVersionResolver::shared(),
            event_bus: None,
            credential_store: None,
        }
    }

    /// Codex カタログ検証に使う client バージョン解決を差し替える (offline tests)。
    #[must_use]
    pub fn with_codex_version_resolver(
        self: std::sync::Arc<Self>,
        resolver: std::sync::Arc<providers::CodexCatalogVersionResolver>,
    ) -> std::sync::Arc<Self> {
        let mut model =
            std::sync::Arc::into_inner(self).expect("freshly composed model has a single owner");
        model.codex_version_resolver = resolver;
        std::sync::Arc::new(model)
    }

    fn resolve(
        &self,
        session_id: &str,
        logical: &LogicalModelId,
        requires_tools: bool,
    ) -> Result<routing::ResolvedRoute, routing::RoutingError> {
        let router = if requires_tools {
            &self.tool_router
        } else {
            &self.router
        };
        router.resolve(
            &mut self
                .affinity
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            session_id,
            logical,
        )
    }
}

impl RoutedModel {
    async fn complete_request(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        tools: &[ToolSpec],
        bus: Option<&EventBus>,
        output_schema: Option<&providers::JsonSchema>,
    ) -> Result<ChatResponse, RuntimeError> {
        let (mut route, generation) = match &invocation.model_preference {
            Some(preference) => {
                // Explicit user selection is authoritative: never apply ADR 0004 fallback.
                let provider =
                    self.providers
                        .get(&preference.profile)
                        .ok_or_else(|| RuntimeError::Model {
                            reason: format!(
                                "selected provider profile `{}` is not configured",
                                preference.profile
                            ),
                        })?;
                let model_id = preference
                    .model
                    .as_ref()
                    .unwrap_or(&provider.profile.default_model);
                if !provider.profile.models.is_empty()
                    && !provider.profile.models.contains(model_id)
                {
                    return Err(RuntimeError::Model {
                        reason: format!(
                            "model `{model_id}` is not listed for profile `{}`",
                            preference.profile
                        ),
                    });
                }
                (
                    routing::ResolvedRoute {
                        profile: preference.profile.clone(),
                        model_id: model_id.clone(),
                    },
                    config::GenerationOverridesConfig::default(),
                )
            }
            None => {
                let binding = self
                    .agents
                    .binding_for(role_key(role), invocation.category.as_deref())
                    .map_err(model_error)?;
                let logical = LogicalModelId::from(binding.logical_model);
                let route = self
                    .resolve(&invocation.run_id, &logical, !tools.is_empty())
                    .map_err(|error| route_resolution_error(error, role, invocation, &logical))?;
                (route, binding.generation)
            }
        };
        let logical = match &invocation.model_preference {
            Some(_) => None,
            None => Some(LogicalModelId::from(
                self.agents
                    .binding_for(role_key(role), invocation.category.as_deref())
                    .map_err(model_error)?
                    .logical_model,
            )),
        };
        let mut fallback_router = if tools.is_empty() {
            &self.router
        } else {
            &self.tool_router
        }
        .clone()
        .with_event_bus(None);
        let mut failures = Vec::new();
        loop {
            let (base_model_id, speed) =
                config::types::provider::parse_model_speed(&route.model_id);
            let provider =
                self.providers
                    .get(&route.profile)
                    .ok_or_else(|| RuntimeError::Model {
                        reason: "resolved provider profile is unavailable".to_string(),
                    })?;
            if !self.verify_candidates().await.contains(&route.profile) {
                let failure = self
                    .verification
                    .get()
                    .and_then(|results| results.get(&route.profile))
                    .and_then(|result| result.as_ref().err())
                    .map_or_else(
                        || "profile was not verified".to_owned(),
                        ToString::to_string,
                    );
                let detail = format!(
                    "profile={} model={}: {failure}",
                    route.profile, route.model_id
                );
                let detail = if provider.auth.api_key.is_empty() {
                    detail
                } else {
                    detail.replace(&provider.auth.api_key, "***")
                };
                let detail: String = detail.chars().take(500).collect();
                if let Some(bus) = bus.or(self.event_bus.as_deref()) {
                    bus.emit(event_bus::Event::new(event_bus::DiagnosticEvent {
                        source: "runtime::compose".into(),
                        severity: event_bus::DiagnosticSeverity::Error,
                        code: event_bus::event::diagnostic_codes::PROVIDER_UNAVAILABLE.into(),
                        detail: detail.clone(),
                        run_id: Some(invocation.run_id.clone()),
                        thread_id: None,
                        call_id: None,
                    }));
                }
                failures.push(detail);
                return Err(RuntimeError::Model {
                    reason: failures.join("; fell back to "),
                });
            }
            let model_allows_tools = match self
                .router
                .catalog()
                .capability_support(base_model_id, Capability::ToolCalling)
            {
                CapabilitySupport::Unsupported => false,
                CapabilitySupport::Supported | CapabilitySupport::Unknown => true,
            };
            let tools = if model_allows_tools && provider.client.capabilities().tool_use {
                tools.to_vec()
            } else {
                Vec::new()
            };
            let mut request = ChatRequest {
                output_schema: output_schema.cloned(),
                model: base_model_id.to_owned(),
                messages: messages.to_vec(),
                tools,
                temperature: generation.temperature,
                max_tokens: generation.max_tokens.map(u64::from),
                reasoning_effort: generation.reasoning_effort.clone(),
                service_tier: match speed {
                    config::types::provider::ModelSpeed::Fast => {
                        Some(providers::ServiceTier::Priority)
                    }
                    config::types::provider::ModelSpeed::Standard => None,
                },
                observation: Some(ObservationContext {
                    run_id: invocation.run_id.clone(),
                }),
            };
            let result = structured::send(provider, &mut request, bus).await;
            let error = match result {
                Ok(response) => return Ok(response),
                Err(error) => error,
            };
            let detail = error.to_string();
            let scrub = |text: &str| {
                if provider.auth.api_key.is_empty() {
                    text.to_owned()
                } else {
                    text.replace(&provider.auth.api_key, "***")
                }
            };
            let provider_error: String = scrub(&detail).chars().take(500).collect();
            let profile_name = scrub(&route.profile);
            let model_id = scrub(&request.model);
            tracing::warn!(profile = %profile_name, model = %model_id, error = %provider_error, "provider request failed");
            failures.push(format!(
                "profile={profile_name} model={model_id}: {provider_error}"
            ));
            let next = logical
                .as_ref()
                .filter(|_| fallback::eligible(&error))
                .and_then(|logical| {
                    fallback_router.exclude_attempt(logical, &route);
                    fallback_router.next_fallback(
                        &mut self
                            .affinity
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner),
                        &invocation.run_id,
                        logical,
                        &route,
                        routing::FailureKind::from(&error),
                        None,
                    )
                });
            let Some(next) = next else {
                return Err(RuntimeError::Model {
                    reason: failures.join("; fell back to "),
                });
            };
            tracing::warn!(run_id = %invocation.run_id, from_provider = %route.profile,
            from_model = %route.model_id, to_provider = %next.profile, to_model = %next.model_id,
            "provider fallback triggered");
            if let (Some(bus), Some(logical)) =
                (bus.or(self.event_bus.as_deref()), logical.as_ref())
            {
                bus.emit(event_bus::Event::new(
                    event_bus::ProviderEvent::FallbackTriggered {
                        from_provider: route.profile.clone(),
                        from_model: Some(route.model_id.clone()),
                        to_provider: next.profile.clone(),
                        to_model: next.model_id.clone(),
                        logical_model: logical.as_str().to_owned(),
                        session_id: invocation.run_id.clone(),
                        failure: routing::FailureKind::from(&error).into(),
                        request_id: None,
                    },
                ));
            }
            route = next;
        }
    }
}

#[async_trait]
impl AgentModel for RoutedModel {
    fn requires_admission(&self) -> bool {
        true
    }

    async fn admit(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
    ) -> Result<(), RuntimeError> {
        self.admit_candidates(invocation, role).await
    }

    async fn complete(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.complete_request(invocation, role, messages, tools, None, None)
            .await
    }

    async fn complete_structured(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        schema: &providers::JsonSchema,
    ) -> Result<ChatResponse, RuntimeError> {
        self.complete_request(invocation, role, messages, &[], None, Some(schema))
            .await
    }

    async fn complete_streaming(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        tools: &[ToolSpec],
        bus: &EventBus,
    ) -> Result<ChatResponse, RuntimeError> {
        self.complete_request(invocation, role, messages, tools, Some(bus), None)
            .await
    }

    fn selected_model(&self, role: Role, category: Option<&str>) -> String {
        let Ok(binding) = self.agents.binding_for(role_key(role), category) else {
            return format!("unresolved:{}", role_key(role));
        };
        let logical = LogicalModelId::from(binding.logical_model);
        self.resolve("runtime-selected-model", &logical, false)
            .map_err(model_error)
            .map(|route| format!("{}/{}", route.profile, route.model_id))
            .unwrap_or_else(|_| format!("unresolved:{}", logical.as_str()))
    }

    fn available_profiles(&self) -> Vec<ProfileSummary> {
        RoutedModel::available_profiles(self)
    }

    fn catalog_context_window(&self, selected_model: &str) -> Option<u64> {
        let (_, model_id) = selected_model.split_once('/')?;
        let (base_model_id, _) = config::types::provider::parse_model_speed(model_id);
        self.router
            .catalog()
            .get(base_model_id)
            .map(|entry| entry.context_window)
            .filter(|window| *window > 0)
    }
}

/// Public, credential-free provider metadata for model pickers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileSummary {
    pub name: String,
    pub provider_type: model::ProviderType,
    pub models: Vec<String>,
    pub default_model: Option<String>,
}

fn route_resolution_error(
    error: routing::RoutingError,
    role: Role,
    invocation: &AgentInvocationContext,
    logical: &LogicalModelId,
) -> RuntimeError {
    let role = role_key(role);
    let category = invocation
        .category
        .as_deref()
        .map_or_else(String::new, |category| format!(", category={category}"));
    let logical = logical.as_str();
    let reason = match &error {
        RoutingError::UnknownLogicalModel(_) => format!(
            "no route configured for logical model `{logical}` (role={role}{category}); \
             add a [[routing.routes.{logical}]] entry: {error}"
        ),
        RoutingError::NoAvailableCandidate(_) => format!(
            "route for logical model `{logical}` exists but has no available candidate \
             (role={role}{category}); check the candidate profiles/models under \
             [[routing.routes.{logical}]]: {error}"
        ),
        _ => format!(
            "could not resolve route for logical model `{logical}` (role={role}{category}); \
             check [[routing.routes.{logical}]]: {error}"
        ),
    };
    RuntimeError::Model {
        reason: reason.chars().take(500).collect(),
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
        Role::Planner => "planner",
        Role::Oracle => "oracle",
        Role::MultimodalLooker => "multimodal_looker",
    }
}

#[cfg(test)]
mod tests;

mod fallback;
mod live;
mod structured;
mod verification;
pub use live::{SwitchableModel, UnconfiguredModel};

pub fn compose_routed_model(
    config: &config::Config,
    deps: ComposeDeps,
) -> Result<Arc<RoutedModel>, CompositionError> {
    let event_bus = deps.event_bus.clone();
    let credential_store = Arc::clone(&deps.credential_store);
    let composed = routing::compose_providers(config, deps).map_err(|error| match error {
        RoutingError::NoProviders => CompositionError::NoProvidersConfigured,
        other => CompositionError::Routing(other),
    })?;
    let mut model = RoutedModel::new(composed, config.agents.clone());
    model.admission_routes = config
        .routing
        .routes
        .values()
        .flatten()
        .filter_map(|candidate| {
            let provider = model.providers.get(&candidate.profile)?;
            Some(routing::ResolvedRoute {
                profile: candidate.profile.clone(),
                model_id: candidate
                    .model
                    .clone()
                    .unwrap_or_else(|| provider.profile.default_model.clone()),
            })
        })
        .collect();
    model.event_bus = event_bus;
    model.credential_store = Some(credential_store);
    Ok(Arc::new(model))
}
