//! 設定済み provider と runtime kernel を接続する edge composition root。
// allow: SIZE_OK — keep the shared provider/runtime composition and route/error path
// together here; source-specific loading lives in its dedicated modules.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use event_bus::EventBus;
use model::{Capability, CapabilitySupport, ModelCatalog};
use providers::{ChatRequest, ChatResponse, Message, ObservationContext, ToolSpec};
use routing::factory::FactoryOptions;
use routing::{ComposeDeps, ComposedProviders, RoutingError, SessionAffinity};
use sandbox::credential::CredentialStore;
use tools::ToolExecutor;

use crate::rules::{ProjectTrust, RulesSettings, RulesSource};
use crate::skill::default_skill_dirs;
use crate::skill_source::SkillCatalogSource;
use crate::{AgentInvocationContext, AgentModel, AgentRuntime, Role, RuntimeError};
mod benchmark;
mod hosted_search;
mod images;
mod output_limits;

/// composition root に production の project root と sandbox factory を渡す seam。
pub struct WorkspaceSeam {
    project_root: PathBuf,
    factory: Arc<dyn crate::SandboxFactory>,
}

impl WorkspaceSeam {
    /// production project root から workspace seam を生成する。
    ///
    /// # Errors
    /// project root を canonicalize できない場合に [`RuntimeError::Workspace`] を返す。
    pub fn production(project_root: PathBuf) -> Result<Self, RuntimeError> {
        Self::with_factory(project_root, Arc::new(crate::network::BwrapFactory))
    }

    /// sandbox factory を明示した workspace seam を生成する。
    ///
    /// bwrap 実行環境を持たない CI でも run ごとの sandbox の結線を検証できるように
    /// するテスト seam。production 経路は [`WorkspaceSeam::production`] を使うこと。
    ///
    /// # Errors
    /// project root を canonicalize できない場合に [`RuntimeError::Workspace`] を返す。
    pub fn with_factory(
        project_root: PathBuf,
        factory: Arc<dyn crate::SandboxFactory>,
    ) -> Result<Self, RuntimeError> {
        let project_root =
            std::fs::canonicalize(&project_root).map_err(|error| RuntimeError::Workspace {
                detail: format!("{}: {error}", project_root.display()),
            })?;
        Ok(Self {
            project_root,
            factory,
        })
    }

    /// canonical な project root を返す。
    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    pub(crate) fn into_factory(self) -> Arc<dyn crate::SandboxFactory> {
        self.factory
    }
}

/// runtime の全外部依存を一度に渡す composition 入力。
pub struct RuntimeComposition<'a> {
    pub config: &'a config::Config,
    /// 明示指定がなければ標準のユーザ config ディレクトリを遅延解決する。
    pub user_config_dir: Option<PathBuf>,
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
    let repo_root = input
        .workspace
        .as_ref()
        .map(|seam| seam.project_root().to_path_buf());
    let user_dir = input
        .user_config_dir
        .clone()
        .or_else(config::user_config_dir);
    let bus = Arc::clone(&input.bus);
    let composed = match input.model_source {
        ModelSource::Fixed(model) => ComposedRuntime {
            runtime: compose_agent_runtime(input.bus, input.executor, model, input.workspace),
            model_identity: ModelIdentity::Fixed,
        },
        ModelSource::Configured => {
            if user_dir.is_none()
                && input.config.providers.values().any(|profile| {
                    profile.provider_type == config::ProviderTypeConfig::AnthropicSubscription
                })
            {
                return Err(RoutingError::InvalidProfile {
                    reason: "Claude installation identity requires a user configuration directory"
                        .into(),
                }
                .into());
            }
            let model = compose_routed_model(
                input.config,
                ComposeDeps {
                    credential_store: Arc::clone(&input.credential_store),
                    event_bus: Some(Arc::clone(&input.bus)),
                    env: input.env,
                    catalog: ModelCatalog::new(),
                    factory: FactoryOptions {
                        claude_installation_id_path: user_dir.as_ref().map(|directory| {
                            directory.join(providers::provider::claude::CLAUDE_INSTALLATION_ID_FILE)
                        }),
                        ..Default::default()
                    },
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
    let runtime = runtime.with_project_rules(Arc::new(RulesSource::new(
        ProjectTrust::Unapproved,
        RulesSettings::from(&input.config.rules),
        None,
        repo_root.clone(),
        user_dir.map(|directory| directory.join("AGENTS.md")),
    )));
    let source = SkillCatalogSource::new(
        input.config.clone(),
        config::user_config_dir(),
        Vec::new(),
        default_skill_dirs(repo_root.as_deref()),
        bus,
    );
    let runtime = runtime.with_skill_source(Arc::new(source));
    runtime.configure_shell_escalation(
        &runtime
            .shared
            .executor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        None,
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
        Some(seam) => AgentRuntime::with_sandbox_factory(bus, executor, model, seam.into_factory()),
        None => AgentRuntime::new(bus, executor, model),
    }
}

/// routing の解決結果を provider request へ変換する AgentModel adapter。
pub struct RoutedModel {
    router: routing::Router,
    tool_router: routing::Router,
    providers: BTreeMap<String, routing::ComposedProvider>,
    affinity: Mutex<SessionAffinity>,
    /// 失敗したプロファイル・モデルのクールダウン。両 Router と共有する。
    health: Arc<routing::ProviderHealth>,
    agents: config::AgentsConfig,
    admission_routes: Vec<routing::ResolvedRoute>,
    /// フォールバックの一周で Retry-After を待つ上限。
    max_retry_after_wait: std::time::Duration,
    /// 設定で決まる context window。キーは `profile/model` とモデル ID。
    configured_windows: BTreeMap<String, u64>,
    /// Configured effort choices by profile, then model ID.
    effort_levels: BTreeMap<String, BTreeMap<String, Vec<String>>>,
    verification:
        tokio::sync::OnceCell<BTreeMap<String, Result<Vec<String>, providers::ProviderError>>>,
    /// Admission reports unverified routes other than the selected one once per composition.
    unverified_reported: std::sync::atomic::AtomicBool,
    event_bus: Option<Arc<EventBus>>,
    provider_configs: BTreeMap<String, config::ProviderProfileConfig>,
    image_input: tokio::sync::OnceCell<BTreeMap<String, bool>>,
    metadata_catalog: tokio::sync::OnceCell<Option<catalog::ModelCatalog>>,
    model_presets: BTreeMap<String, config::ModelPresetConfig>,
}

impl RoutedModel {
    /// Returns credential-free profiles in name order. Image models are left out: a
    /// picker selects a conversation model, and image models only serve the
    /// image generator (ADR 0038).
    pub fn available_profiles(&self) -> Vec<ProfileSummary> {
        self.providers
            .iter()
            .map(|(name, provider)| ProfileSummary {
                name: name.clone(),
                provider_type: provider.profile.provider_type,
                models: provider
                    .profile
                    .models
                    .iter()
                    .filter(|model| self.model_kind(name, model) == config::ModelKind::Chat)
                    .cloned()
                    .collect(),
                default_model: (self.model_kind(name, &provider.profile.default_model)
                    == config::ModelKind::Chat)
                    .then(|| provider.profile.default_model.clone()),
                effort_levels: self.effort_levels.get(name).cloned().unwrap_or_default(),
            })
            .collect()
    }

    pub fn new(composed: ComposedProviders, agents: config::AgentsConfig) -> Self {
        let health = Arc::new(routing::ProviderHealth::default());
        Self {
            tool_router: composed
                .router
                .clone()
                .requiring_capability(Capability::ToolCalling)
                .with_health(Arc::clone(&health)),
            router: composed.router.with_health(Arc::clone(&health)),
            health,
            providers: composed.providers,
            affinity: Mutex::new(SessionAffinity::default()),
            agents,
            admission_routes: Vec::new(),
            max_retry_after_wait: std::time::Duration::from_secs(
                config::FallbackConfig::default().max_retry_after_wait_secs,
            ),
            configured_windows: BTreeMap::new(),
            effort_levels: BTreeMap::new(),
            verification: tokio::sync::OnceCell::new(),
            unverified_reported: std::sync::atomic::AtomicBool::new(false),
            event_bus: None,
            provider_configs: BTreeMap::new(),
            image_input: tokio::sync::OnceCell::new(),
            metadata_catalog: tokio::sync::OnceCell::new(),
            model_presets: BTreeMap::new(),
        }
    }

    async fn accepts_images(
        &self,
        messages: &[Message],
        profile: &str,
        base_model_id: &str,
    ) -> bool {
        if !images::has_tool_result_images(messages) {
            return true;
        }
        let models = self
            .image_input
            .get_or_init(|| async {
                if self.provider_configs.is_empty() {
                    return BTreeMap::new();
                }
                match self.metadata_catalog().await {
                    Some(catalog) => {
                        crate::model_resolve::image_input_models(&self.provider_configs, catalog)
                    }
                    None => BTreeMap::new(),
                }
            })
            .await;
        models
            .get(&format!("{profile}/{base_model_id}"))
            .copied()
            .unwrap_or(true)
    }

    /// クールダウンの既定期間を設定し、空の状態から始める。
    #[must_use]
    pub fn with_cooldown(mut self, policy: config::CooldownConfig) -> Self {
        self.health = Arc::new(routing::ProviderHealth::new(policy));
        self.router = self.router.with_health(Arc::clone(&self.health));
        self.tool_router = self.tool_router.with_health(Arc::clone(&self.health));
        self
    }

    fn router_for(&self, requires_tools: bool) -> &routing::Router {
        if requires_tools {
            &self.tool_router
        } else {
            &self.router
        }
    }

    /// ロールのバインディングからチェーンを展開し、セッションのピン (なければ先頭) を解決する。
    fn resolve_binding(
        &self,
        session_id: &str,
        role: Role,
        category: Option<&str>,
        requires_tools: bool,
    ) -> Result<(config::types::agents::ResolvedAgentBinding, ChainChoice), RuntimeError> {
        self.resolve_binding_in(
            &mut self
                .affinity
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            session_id,
            role,
            category,
            requires_tools,
        )
    }

    /// [`Self::resolve_binding`] を、指定したアフィニティ上で行う。
    fn resolve_binding_in(
        &self,
        affinity: &mut SessionAffinity,
        session_id: &str,
        role: Role,
        category: Option<&str>,
        requires_tools: bool,
    ) -> Result<(config::types::agents::ResolvedAgentBinding, ChainChoice), RuntimeError> {
        let binding = self
            .agents
            .binding_for(role_key(role), category)
            .map_err(model_error)?;
        let router = self.router_for(requires_tools);
        let resolution_error =
            |error| route_resolution_error(error, role, category, &binding.models);
        let chain = router
            .chain(chain_key(role, category), &binding.models)
            .map_err(resolution_error)?;
        self.ensure_chat_chain(&chain)?;
        let position = router
            .resolve_chain(affinity, session_id, &chain)
            .map_err(resolution_error)?;
        Ok((binding, ChainChoice { chain, position }))
    }

    /// The kind of `model` on `profile`: its registration, else the ID (ADR 0038).
    fn model_kind(&self, profile: &str, model: &str) -> config::ModelKind {
        self.provider_configs.get(profile).map_or_else(
            || config::ModelKind::infer(model),
            |config| config.model_kind(model),
        )
    }

    /// Refuses a conversation or summary chain with a member that is not a chat model,
    /// before anything is sent (ADR 0038). Like a missing route, the whole chain fails
    /// when it is used, whichever member would answer.
    fn ensure_chat_chain(&self, chain: &routing::Chain) -> Result<(), RuntimeError> {
        for member in chain.members() {
            let kind = self.model_kind(&member.target.profile, &member.target.model_id);
            if kind != config::ModelKind::Chat {
                return Err(RuntimeError::ModelKindMismatch {
                    origin: format!("chain `{}` route `{}`", chain.key(), member.route),
                    profile: member.target.profile.clone(),
                    model: member.target.model_id.clone(),
                    kind,
                });
            }
        }
        Ok(())
    }

    /// 要約に使うメンバーを選ぶ (ADR 0030)。
    ///
    /// `agents.roles.compactor` のチェーンがあればそれを、なければ実行中ロールのチェーンを使い、
    /// context window が入力より大きいメンバーだけを残す。run のピン先が残っていればそこから
    /// 始めるが、要約の結果でピンは動かさない。
    fn resolve_summary_invocation(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ResolvedInvocation, RuntimeError> {
        let category = invocation.category.as_deref();
        let compactor = &self.agents.roles.compactor;
        let (key, models, generation) = if compactor.models.is_empty() {
            let binding = self
                .agents
                .binding_for(role_key(role), category)
                .map_err(model_error)?;
            (
                chain_key(role, category),
                binding.models,
                binding.generation,
            )
        } else {
            (
                "compactor".to_owned(),
                compactor.models.clone(),
                compactor.generation.clone(),
            )
        };
        let needed = crate::compaction::estimator::estimate_tokens(messages)
            .saturating_add(crate::compaction::estimator::estimate_tool_tokens(tools));
        let router = self.router_for(!tools.is_empty());
        let mut scratch = self
            .affinity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let resolution_error = |error| route_resolution_error(error, role, category, &models);
        let chain = router.chain(key, &models).map_err(resolution_error)?;
        self.ensure_chat_chain(&chain)?;
        let chain = chain.filtered(|member| {
            self.context_window(&member.target)
                .is_none_or(|window| window > needed)
        });
        let position = router
            .resolve_chain(&mut scratch, &invocation.run_id, &chain)
            .map_err(resolution_error)?;
        let choice = ChainChoice { chain, position };
        let member = choice.chain.member(choice.position);
        Ok(ResolvedInvocation {
            route: member.target.clone(),
            reasoning_effort: member.reasoning_effort.clone(),
            generation,
            chain: Some(choice),
            pin_on_success: false,
            pinned_before: None,
        })
    }

    /// 具体モデルの context window。設定を優先し、なければカタログを見る。不明なら `None`。
    fn context_window(&self, route: &routing::ResolvedRoute) -> Option<u64> {
        let (base_model_id, _) = config::types::provider::parse_model_speed(&route.model_id);
        self.configured_windows
            .get(&format!("{}/{base_model_id}", route.profile))
            .or_else(|| self.configured_windows.get(base_model_id))
            .copied()
            .or_else(|| {
                self.router
                    .catalog()
                    .get(base_model_id)
                    .map(|entry| entry.context_window)
            })
            .filter(|window| *window > 0)
    }
}

impl RoutedModel {
    // 通常生成と公式 compaction で override / role / category の解決を揃える。
    fn resolve_invocation(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        tools: &[ToolSpec],
    ) -> Result<ResolvedInvocation, RuntimeError> {
        Ok(match &invocation.model_preference {
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
                let kind = self.model_kind(&preference.profile, model_id);
                if kind != config::ModelKind::Chat {
                    return Err(RuntimeError::ModelKindMismatch {
                        origin: "the selected model".to_owned(),
                        profile: preference.profile.clone(),
                        model: model_id.clone(),
                        kind,
                    });
                }
                ResolvedInvocation {
                    route: routing::ResolvedRoute {
                        profile: preference.profile.clone(),
                        model_id: model_id.clone(),
                    },
                    generation: config::GenerationOverridesConfig::default(),
                    reasoning_effort: preference.reasoning_effort.clone(),
                    chain: None,
                    pin_on_success: false,
                    pinned_before: None,
                }
            }
            None => {
                let pinned_before = self
                    .affinity
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .pinned(
                        &invocation.run_id,
                        &chain_key(role, invocation.category.as_deref()),
                    )
                    .map(|pin| (pin.profile.clone(), pin.model.clone()));
                let (binding, choice) = self.resolve_binding(
                    &invocation.run_id,
                    role,
                    invocation.category.as_deref(),
                    !tools.is_empty(),
                )?;
                let member = choice.chain.member(choice.position);
                ResolvedInvocation {
                    route: member.target.clone(),
                    reasoning_effort: member.reasoning_effort.clone(),
                    generation: binding.generation,
                    chain: Some(choice),
                    pin_on_success: true,
                    pinned_before,
                }
            }
        })
    }

    /// 失敗をクールダウンへ記録し、開始・延長したら観測イベントを発行する。
    fn record_cooldown(
        &self,
        route: &routing::ResolvedRoute,
        failure: routing::FailureKind,
        retry_after: Option<std::time::Duration>,
        run_id: &str,
        bus: Option<&EventBus>,
    ) {
        let Some(cooldown) = self.health.record_failure(
            &route.profile,
            &route.model_id,
            failure,
            retry_after,
            std::time::Instant::now(),
        ) else {
            return;
        };
        tracing::warn!(profile = %cooldown.profile, model = ?cooldown.model,
            duration = ?cooldown.duration, ?failure, "provider cooldown started");
        if let Some(bus) = bus.or(self.event_bus.as_deref()) {
            bus.emit(event_bus::Event::new(
                event_bus::ProviderEvent::CooldownStarted {
                    profile: cooldown.profile,
                    model: cooldown.model,
                    failure: failure.into(),
                    cooldown_ms: cooldown
                        .duration
                        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)),
                    run_id: Some(run_id.to_owned()),
                },
            ));
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn complete_request(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        tools: &[ToolSpec],
        bus: Option<&EventBus>,
        output_schema: Option<&providers::JsonSchema>,
        benchmark: Option<&crate::benchmark::BenchmarkModelSettings>,
    ) -> Result<ChatResponse, RuntimeError> {
        let summary = invocation.purpose == event_bus::RequestPurpose::Compaction
            && invocation.model_preference.is_none();
        let ResolvedInvocation {
            mut route,
            generation,
            mut reasoning_effort,
            chain,
            pin_on_success,
            pinned_before,
        } = if summary {
            self.resolve_summary_invocation(invocation, role, messages, tools)?
        } else {
            self.resolve_invocation(invocation, role, tools)?
        };
        let carries_compaction = messages
            .iter()
            .flat_map(|message| &message.content)
            .any(|block| matches!(block, providers::ContentBlock::Compaction { .. }));
        let generation = benchmark.map_or(generation, |settings| settings.generation.clone());
        let mut position = chain.as_ref().map(|choice| choice.position);
        let chain = chain.map(|choice| choice.chain);
        let router = self.router_for(!tools.is_empty());
        let mut cycle: Option<routing::FallbackCycle> = None;
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
                let next = match (chain.as_ref(), position) {
                    (Some(chain), Some(failed)) => {
                        let step = FallbackAttempt {
                            failed,
                            from: &route,
                            failure: routing::FailureKind::Other,
                            transient: false,
                            retry_after: None,
                        };
                        self.advance_fallback(router, chain, &mut cycle, step, invocation, bus)
                            .await
                    }
                    _ => None,
                };
                if let Some(bus) = bus.or(self.event_bus.as_deref()) {
                    bus.emit(event_bus::Event::new(event_bus::DiagnosticEvent {
                        source: "runtime::compose".into(),
                        severity: if next.is_some() {
                            event_bus::DiagnosticSeverity::Warning
                        } else {
                            event_bus::DiagnosticSeverity::Error
                        },
                        code: event_bus::event::diagnostic_codes::PROVIDER_UNAVAILABLE.into(),
                        detail: detail.clone(),
                        run_id: Some(invocation.run_id.clone()),
                        thread_id: None,
                        call_id: None,
                    }));
                }
                failures.push(detail);
                let (Some(chain), Some(next)) = (chain.as_ref(), next) else {
                    return Err(RuntimeError::Model {
                        reason: failures.join("; fell back to "),
                    });
                };
                let member = chain.member(next);
                route = member.target.clone();
                reasoning_effort = member.reasoning_effort.clone();
                position = Some(next);
                continue;
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
            if benchmark.is_some() && tools.is_empty() {
                return Err(crate::benchmark::unsupported(
                    "candidate cannot preserve tool calling",
                ));
            }
            if invocation.purpose == event_bus::RequestPurpose::Agent {
                // A switch is a cache boundary (ADR 0030): the agent loop re-summarizes a
                // checkpoint the target cannot replay and, knowing every configured window,
                // compacts for a smaller one before the request is sent again.
                let resummarize = carries_compaction && !provider.client.replays_compaction();
                let switching = pinned_before.as_ref().is_some_and(|(profile, model)| {
                    *profile != route.profile || *model != route.model_id
                });
                if resummarize || switching {
                    if let (Some(chain), Some(position)) = (chain.as_ref(), position) {
                        routing::pin(
                            &mut self
                                .affinity
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner),
                            &invocation.run_id,
                            chain,
                            position,
                        );
                    }
                    tracing::info!(run_id = %invocation.run_id, profile = %route.profile,
                        model = %route.model_id, resummarize,
                        "preparing the context before switching models");
                    return Err(RuntimeError::ModelSwitch {
                        selected: format!("{}/{}", route.profile, route.model_id),
                        resummarize,
                    });
                }
            }
            let accepts_images = self
                .accepts_images(messages, &route.profile, base_model_id)
                .await;
            let mut request = ChatRequest {
                output_schema: output_schema.cloned(),
                model: base_model_id.to_owned(),
                messages: images::for_model(messages, &tools, accepts_images),
                tools,
                temperature: generation.temperature,
                max_tokens: if benchmark.is_some() {
                    generation.max_tokens.map(u64::from)
                } else {
                    self.output_limit(&route, generation.max_tokens).await
                },
                reasoning_effort: reasoning_effort.clone(),
                service_tier: benchmark.map_or_else(
                    || match speed {
                        config::types::provider::ModelSpeed::Fast => {
                            Some(providers::ServiceTier::Priority)
                        }
                        config::types::provider::ModelSpeed::Standard => None,
                    },
                    |settings| settings.service_tier,
                ),
                observation: Some(ObservationContext {
                    turn: invocation.turn.clone(),
                    run_id: invocation.run_id.clone(),
                    purpose: invocation.purpose,
                }),
            };
            let result = structured::send(provider, &mut request, bus).await;
            let error = match result {
                Ok(response) => {
                    if let (true, Some(chain), Some(position)) =
                        (pin_on_success, chain.as_ref(), position)
                    {
                        routing::pin(
                            &mut self
                                .affinity
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner),
                            &invocation.run_id,
                            chain,
                            position,
                        );
                    }
                    return Ok(response);
                }
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
            let failure = routing::FailureKind::classify(&error, provider.profile.provider_type);
            self.record_cooldown(
                &route,
                failure,
                routing::retry_after(&error),
                &invocation.run_id,
                bus,
            );
            let exhausted = || RuntimeError::Model {
                reason: failures.join("; fell back to "),
            };
            let (Some(chain), Some(failed)) = (chain.as_ref(), position) else {
                return Err(exhausted());
            };
            if !fallback::eligible(&error) {
                return Err(exhausted());
            }
            let step = FallbackAttempt {
                failed,
                from: &route,
                failure,
                transient: routing::is_transient(&error),
                retry_after: routing::retry_after(&error),
            };
            let Some(next) = self
                .advance_fallback(router, chain, &mut cycle, step, invocation, bus)
                .await
            else {
                return Err(exhausted());
            };
            let member = chain.member(next);
            route = member.target.clone();
            reasoning_effort = member.reasoning_effort.clone();
            position = Some(next);
        }
    }

    /// Records `step` in this request's one-pass fallback and returns the next member,
    /// or `None` once the pool is exhausted.
    async fn advance_fallback(
        &self,
        router: &routing::Router,
        chain: &routing::Chain,
        cycle: &mut Option<routing::FallbackCycle>,
        step: FallbackAttempt<'_>,
        invocation: &AgentInvocationContext,
        bus: Option<&EventBus>,
    ) -> Option<usize> {
        let cycle = cycle.get_or_insert_with(|| routing::FallbackCycle::new(chain, step.failed));
        cycle.record_failure(step.failed, step.transient, step.retry_after);
        let next = loop {
            match cycle.next(router, chain, self.max_retry_after_wait) {
                routing::FallbackStep::Try(next) => break next,
                routing::FallbackStep::Wait(wait) => {
                    tracing::info!(run_id = %invocation.run_id, wait = ?wait,
                        "waiting for Retry-After before retrying the route pool");
                    tokio::time::sleep(wait).await;
                }
                routing::FallbackStep::Exhausted => return None,
            }
        };
        let member = chain.member(next);
        let from = step.from;
        tracing::warn!(run_id = %invocation.run_id, from_provider = %from.profile,
        from_model = %from.model_id, to_provider = %member.target.profile,
        to_model = %member.target.model_id, "provider fallback triggered");
        if let Some(bus) = bus.or(self.event_bus.as_deref()) {
            bus.emit(event_bus::Event::new(
                event_bus::ProviderEvent::FallbackTriggered {
                    from_provider: from.profile.clone(),
                    from_model: Some(from.model_id.clone()),
                    to_provider: member.target.profile.clone(),
                    to_model: member.target.model_id.clone(),
                    logical_model: member.route.clone(),
                    session_id: invocation.run_id.clone(),
                    failure: step.failure.into(),
                    request_id: None,
                },
            ));
        }
        Some(next)
    }
}

struct FallbackAttempt<'a> {
    failed: usize,
    from: &'a routing::ResolvedRoute,
    failure: routing::FailureKind,
    transient: bool,
    retry_after: Option<std::time::Duration>,
}

#[async_trait]
impl AgentModel for RoutedModel {
    fn web_search_provider(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        tools: &[ToolSpec],
        usage_sink: Arc<dyn Fn(providers::Usage) + Send + Sync>,
    ) -> Result<Option<Arc<dyn tools::search::SearchProvider>>, RuntimeError> {
        hosted_search::resolve(self, invocation, role, tools, usage_sink)
    }

    async fn benchmark_settings(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        tools: &[ToolSpec],
    ) -> Result<crate::benchmark::BenchmarkModelSettings, RuntimeError> {
        let ResolvedInvocation {
            route,
            mut generation,
            reasoning_effort,
            ..
        } = self.resolve_invocation(invocation, role, tools)?;
        let (_, speed) = config::types::provider::parse_model_speed(&route.model_id);
        let protocol = self.providers[&route.profile].profile.api_protocol;
        let max_tokens = self
            .output_limit(&route, generation.max_tokens)
            .await
            .or_else(|| {
                (protocol == model::ApiProtocol::AnthropicMessages)
                    .then_some(providers::wire::anthropic::DEFAULT_MAX_TOKENS)
            });
        generation.max_tokens = max_tokens.map(u32::try_from).transpose().map_err(|_| {
            crate::benchmark::unsupported("effective max_tokens exceeds benchmark generation range")
        })?;
        Ok(crate::benchmark::BenchmarkModelSettings {
            protocol,
            preference: crate::ModelPreference {
                profile: route.profile,
                model: Some(route.model_id.clone()),
                reasoning_effort,
            },
            generation,
            service_tier: match speed {
                config::types::provider::ModelSpeed::Fast => Some(providers::ServiceTier::Priority),
                config::types::provider::ModelSpeed::Standard => None,
            },
        })
    }

    fn freeze_for_benchmark(
        self: Arc<Self>,
        settings: crate::benchmark::BenchmarkModelSettings,
    ) -> Result<Arc<dyn AgentModel>, RuntimeError> {
        benchmark::freeze(self, settings)
    }
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

    async fn compact_context(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<Option<providers::CompactionResult>, RuntimeError> {
        // 解決は一度だけ。provider の失敗を別 route で再試行せず Summarizer へ返す。
        let ResolvedInvocation {
            route,
            generation,
            reasoning_effort,
            ..
        } = self.resolve_invocation(invocation, role, tools)?;
        let provider = self
            .providers
            .get(&route.profile)
            .ok_or_else(|| RuntimeError::Model {
                reason: "resolved provider profile is unavailable".to_string(),
            })?;
        let Some(compactor) = provider.client.compactor() else {
            return Ok(None);
        };
        let (base_model_id, speed) = config::types::provider::parse_model_speed(&route.model_id);
        let model_allows_tools = !matches!(
            self.router
                .catalog()
                .capability_support(base_model_id, Capability::ToolCalling),
            CapabilitySupport::Unsupported
        );
        let tools = if model_allows_tools && provider.client.capabilities().tool_use {
            tools.to_vec()
        } else {
            Vec::new()
        };
        let accepts_images = self
            .accepts_images(messages, &route.profile, base_model_id)
            .await;
        let request = ChatRequest {
            model: base_model_id.to_owned(),
            messages: images::for_model(messages, &tools, accepts_images),
            tools,
            temperature: generation.temperature,
            max_tokens: self.output_limit(&route, generation.max_tokens).await,
            reasoning_effort,
            service_tier: match speed {
                config::types::provider::ModelSpeed::Fast => Some(providers::ServiceTier::Priority),
                config::types::provider::ModelSpeed::Standard => None,
            },
            output_schema: None,
            observation: Some(ObservationContext {
                turn: invocation.turn.clone(),
                run_id: invocation.run_id.clone(),
                purpose: event_bus::RequestPurpose::Compaction,
            }),
        };
        compactor
            .compact(&request)
            .await
            .map(Some)
            .map_err(model_error)
    }

    async fn complete(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.complete_request(invocation, role, messages, tools, None, None, None)
            .await
    }

    async fn complete_structured(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        schema: &providers::JsonSchema,
    ) -> Result<ChatResponse, RuntimeError> {
        self.complete_request(invocation, role, messages, &[], None, Some(schema), None)
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
        self.complete_request(invocation, role, messages, tools, Some(bus), None, None)
            .await
    }

    fn selected_model(&self, role: Role, category: Option<&str>) -> String {
        let Ok(binding) = self.agents.binding_for(role_key(role), category) else {
            return format!("unresolved:{}", role_key(role));
        };
        self.resolve_binding("runtime-selected-model", role, category, false)
            .map(|(_, choice)| {
                let route = &choice.chain.member(choice.position).target;
                format!("{}/{}", route.profile, route.model_id)
            })
            .unwrap_or_else(|_| format!("unresolved:{}", binding.head_route()))
    }

    fn selected_model_for_run(&self, run_id: &str, role: Role, category: Option<&str>) -> String {
        let mut scratch = self
            .affinity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        match self.resolve_binding_in(&mut scratch, run_id, role, category, false) {
            Ok((_, choice)) => {
                let route = &choice.chain.member(choice.position).target;
                format!("{}/{}", route.profile, route.model_id)
            }
            Err(_) => self.selected_model(role, category),
        }
    }

    fn selected_reasoning_effort(&self, role: Role, category: Option<&str>) -> Option<String> {
        let (_, choice) = self
            .resolve_binding("runtime-selected-model", role, category, false)
            .ok()?;
        choice
            .chain
            .member(choice.position)
            .reasoning_effort
            .clone()
    }

    fn available_profiles(&self) -> Vec<ProfileSummary> {
        RoutedModel::available_profiles(self)
    }

    fn route_pins(&self, run_id: &str) -> Vec<crate::RoutePinRecord> {
        self.affinity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pins(run_id)
            .into_iter()
            .map(|(chain, pin)| crate::RoutePinRecord {
                chain,
                position: pin.position,
                profile: pin.profile,
                model: pin.model,
            })
            .collect()
    }

    fn route_pins_by_run(&self) -> BTreeMap<String, Vec<crate::RoutePinRecord>> {
        let sessions = self
            .affinity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .sessions();
        sessions
            .into_iter()
            .map(|run_id| {
                let pins = self.route_pins(&run_id);
                (run_id, pins)
            })
            .collect()
    }

    fn restore_route_pins(&self, run_id: &str, pins: &[crate::RoutePinRecord]) {
        let mut affinity = self
            .affinity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for pin in pins {
            affinity.pin(
                run_id,
                &normalize_chain_key(&pin.chain),
                routing::RoutePin {
                    position: pin.position,
                    profile: pin.profile.clone(),
                    model: pin.model.clone(),
                },
            );
        }
    }

    fn binding_preview(
        &self,
        role: Role,
        category: Option<&str>,
    ) -> Option<config::types::agents::ResolvedAgentBinding> {
        self.agents.binding_for(role_key(role), category).ok()
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
    /// Configured effort choices by model ID; absent models use the common defaults.
    pub effort_levels: BTreeMap<String, Vec<String>>,
}

/// Route and request settings resolved for one invocation.
struct ResolvedInvocation {
    route: routing::ResolvedRoute,
    generation: config::GenerationOverridesConfig,
    /// Explicit selection's effort, or the chain entry's.
    reasoning_effort: Option<String>,
    /// The role's chain and selected member; `None` for an explicit selection.
    chain: Option<ChainChoice>,
    /// Whether the member that answers becomes the run's pin; summaries leave it alone.
    pin_on_success: bool,
    /// The run's pinned (profile, model) before this request, to detect a switch.
    pinned_before: Option<(String, String)>,
}

/// A role's expanded chain and the member chosen for this request.
struct ChainChoice {
    chain: routing::Chain,
    position: usize,
}

/// Saved affinity keys identify roles, not user-defined routing.routes names.
/// Normalize only the role segment; a category named `oracle` stays unchanged.
fn normalize_chain_key(key: &str) -> String {
    let (role, category) = key
        .split_once('/')
        .map_or((key, None), |(role, category)| (role, Some(category)));
    if role == "oracle" {
        chain_key(Role::Advisor, category)
    } else {
        key.to_owned()
    }
}

/// Affinity key of a role's chain: the role, or role and category.
fn chain_key(role: Role, category: Option<&str>) -> String {
    match category {
        Some(category) => format!("{}/{category}", role_key(role)),
        None => role_key(role).to_owned(),
    }
}

fn route_resolution_error(
    error: routing::RoutingError,
    role: Role,
    category: Option<&str>,
    chain: &[config::ChainEntryConfig],
) -> RuntimeError {
    let role = role_key(role);
    let category = category.map_or_else(String::new, |category| format!(", category={category}"));
    let tables = chain
        .iter()
        .map(|entry| format!("[[routing.routes.{}]]", entry.route))
        .collect::<Vec<_>>()
        .join(", ");
    let reason = match &error {
        RoutingError::UnknownLogicalModel(route) => format!(
            "no route configured for `{route}` in the model chain (role={role}{category}); \
             add a [[routing.routes.{route}]] entry: {error}"
        ),
        RoutingError::NoAvailableCandidate(_) => format!(
            "the model chain exists but has no available candidate (role={role}{category}); \
             check the candidate profiles/models under {tables}: {error}"
        ),
        _ => format!(
            "could not resolve the model chain (role={role}{category}); check {tables}: {error}"
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
        Role::WebResearcher => "web_researcher",
        Role::Planner => "planner",
        Role::Advisor => "advisor",
        Role::MultimodalLooker => "multimodal_looker",
        Role::Evaluator => "evaluator",
    }
}

#[cfg(test)]
mod tests;

mod fallback;
mod live;
mod structured;
mod verification;
pub use live::{SwitchableModel, UnconfiguredModel};

/// Windows fixed by configuration: `[compaction.model_overrides]`, then each model's
/// manual or preset window. The catalog and provider listings fill in the rest.
fn configured_windows(config: &config::Config) -> BTreeMap<String, u64> {
    let mut windows = config.compaction.model_overrides.clone();
    for (name, profile) in &config.providers {
        for entry in profile.models.iter().filter(|entry| entry.enabled) {
            let resolved = crate::model_resolve::resolve_model_metadata(
                entry,
                &config.model_presets,
                None,
                Some(name),
                Some(profile.provider_type),
            );
            if let Some(window) = resolved.context_window {
                windows
                    .entry(format!("{name}/{}", entry.id))
                    .or_insert(window);
                windows.entry(entry.id.clone()).or_insert(window);
            }
        }
    }
    windows.retain(|_, window| *window > 0);
    windows
}

pub fn compose_routed_model(
    config: &config::Config,
    deps: ComposeDeps,
) -> Result<Arc<RoutedModel>, CompositionError> {
    let event_bus = deps.event_bus.clone();
    let composed = routing::compose_providers(config, deps).map_err(|error| match error {
        RoutingError::NoProviders => CompositionError::NoProvidersConfigured,
        other => CompositionError::Routing(other),
    })?;
    let mut model = RoutedModel::new(composed, config.agents.clone())
        .with_cooldown(config.routing.cooldown.clone());
    // Image routes serve the image generator, not conversations, so admission never
    // reports them as unverified conversation fallbacks (ADR 0038).
    model.admission_routes = config
        .routing
        .routes
        .values()
        .flatten()
        .filter(|candidate| candidate.kind(&config.providers) == Some(config::ModelKind::Chat))
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
    model.effort_levels = config
        .providers
        .iter()
        .map(|(name, profile)| (name.clone(), profile.effort_levels_by_model()))
        .collect();
    model.max_retry_after_wait =
        std::time::Duration::from_secs(config.routing.fallback.max_retry_after_wait_secs);
    model.configured_windows = configured_windows(config);
    model.provider_configs = config.providers.clone();
    model.model_presets = config.model_presets.clone();
    model.event_bus = event_bus;
    Ok(Arc::new(model))
}
