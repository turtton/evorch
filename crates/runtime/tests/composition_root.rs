mod support;

use std::collections::BTreeMap;
use std::sync::Arc;

use config::{
    ApiProtocolConfig, Config, CredentialRefConfig, ProviderProfileConfig, ProviderTypeConfig,
};
use event_bus::EventBus;
use model::ModelCatalog;
use routing::factory::FactoryOptions;
use routing::{ComposeDeps, MapEnv, compose_providers};
use runtime::{
    AgentModel, AgentRunPhase, ExecutionPolicy, ModelIdentity, ModelSource, Role, RoutedModel,
    RunConfig, RuntimeComposition, SandboxFactory, WorkspaceMounts, WorkspaceSeam, compose_runtime,
};
use sandbox::credential::{CredentialStore, FileCredentialStore};
use sandbox::{DirectSandbox, Sandbox, SandboxError};
use tools::ToolExecutor;

use support::{ScriptedModel, text_response};

const PROFILE: &str = "local";
const MODEL: &str = "local-model";
const KEY_ENV: &str = "EVORCH_TEST_KEY_COMPOSITION_ROOT";

struct DirectSandboxFactory;

impl SandboxFactory for DirectSandboxFactory {
    fn build(
        &self,
        _policy: &ExecutionPolicy,
        _mounts: &WorkspaceMounts,
    ) -> Result<Arc<dyn Sandbox>, SandboxError> {
        Ok(Arc::new(DirectSandbox::new_unchecked()))
    }
}

fn credential_store() -> Arc<dyn CredentialStore> {
    let directory = tempfile::tempdir().expect("credential directory");
    Arc::new(FileCredentialStore::open(directory.path()).expect("credential store"))
}

fn executor(bus: &Arc<EventBus>) -> Arc<ToolExecutor> {
    Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ))
}

fn configured() -> Config {
    Config {
        providers: BTreeMap::from([(
            PROFILE.to_string(),
            ProviderProfileConfig {
                provider_type: ProviderTypeConfig::OpenAiCompatible,
                api_protocol: ApiProtocolConfig::OpenAiCompletions,
                base_url: "http://127.0.0.1:1/v1".to_string(),
                credential: CredentialRefConfig::Env {
                    var: KEY_ENV.to_string(),
                },
                models: vec![config::ModelEntryConfig::enabled(MODEL)],
                excluded_models: Vec::new(),
                default_model: MODEL.to_string(),
            },
        )]),
        routing: config::RoutingConfig {
            cooldown: Default::default(),
            fallback: Default::default(),
            routes: ["orchestrator", "explorer", "worker", "reviewer"]
                .into_iter()
                .map(|logical| {
                    (
                        logical.to_string(),
                        vec![config::RouteCandidateConfig {
                            profile: PROFILE.to_string(),
                            model: None,
                            auto: false,
                        }],
                    )
                })
                .collect(),
        },
        ..Config::default()
    }
}

fn composition<'a>(
    config: &'a Config,
    bus: Arc<EventBus>,
    model_source: ModelSource,
    workspace: Option<WorkspaceSeam>,
) -> RuntimeComposition<'a> {
    RuntimeComposition {
        user_config_dir: Some(
            tempfile::tempdir()
                .expect("user config directory")
                .path()
                .join("empty-user-config"),
        ),
        config,
        executor: executor(&bus),
        bus,
        credential_store: credential_store(),
        env: Arc::new(MapEnv::from_iter([(KEY_ENV, "test-key")])),
        model_source,
        workspace,
    }
}

// Given: provider を持たない既定 Config と固定 ScriptedModel
// When: compose_runtime で runtime を一度だけ構築する
// Then: provider 構成を要求せず Fixed identity の runtime が得られ、user AGENTS を注入する
#[tokio::test]
async fn fixed_source_ignores_missing_providers() {
    let config = Config::default();
    let bus = Arc::new(EventBus::new(32));
    let directory = tempfile::tempdir().expect("user config directory");
    let body = "FIXED-MODEL-USER-AGENTS";
    std::fs::write(directory.path().join("AGENTS.md"), body).expect("user AGENTS.md");
    let model = Arc::new(ScriptedModel::new([Ok(text_response(
        "fixed done",
        providers::FinishReason::Stop,
    ))]));
    let mut input = composition(&config, bus, ModelSource::Fixed(model.clone()), None);
    input.user_config_dir = Some(directory.path().to_path_buf());
    let composed = compose_runtime(input).expect("fixed model composes without providers");

    assert!(matches!(composed.model_identity, ModelIdentity::Fixed));
    assert!(composed.runtime.list_agents().is_empty());
    let run = composed.runtime.delegate_background(
        Role::Worker,
        "fixed".to_string(),
        RunConfig::default(),
    );
    assert_eq!(composed.runtime.wait(run).await, Ok(AgentRunPhase::Done));
    assert_eq!(
        composed.runtime.run_result(run),
        Ok(Some("fixed done".to_string()))
    );
    let observed = model.observed().await;
    let system = observed[0]
        .iter()
        .filter(|message| message.role == providers::Role::System)
        .collect::<Vec<_>>();
    assert_eq!(system.len(), 1);
    assert!(system[0].content.iter().any(|block| matches!(block,
        providers::ContentBlock::Text { text } if text.contains(body))));
}

// Given: provider を持たない既定 Config と Configured source
// When: compose_runtime する
// Then: edge composition root 固有の NoProvidersConfigured で fail-closed になる
#[test]
fn configured_source_rejects_missing_providers() {
    let config = Config::default();
    let bus = Arc::new(EventBus::new(32));
    let error = match compose_runtime(composition(&config, bus, ModelSource::Configured, None)) {
        Ok(_) => panic!("configured source requires providers"),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        runtime::CompositionError::NoProvidersConfigured
    ));
}

// Given: 同じ Config/依存で runtime composition と routing composition を構築する
// When: 全 runtime role の selected_model を比較する
// Then: edge root と直接 adapter の選択結果が一致する
#[test]
fn composed_runtime_and_direct_routed_model_have_selected_model_parity() {
    let config = configured();
    let bus = Arc::new(EventBus::new(32));
    let composed = compose_runtime(composition(
        &config,
        Arc::clone(&bus),
        ModelSource::Configured,
        None,
    ))
    .expect("runtime composition succeeds");
    let direct = RoutedModel::new(
        compose_providers(
            &config,
            ComposeDeps {
                credential_store: credential_store(),
                event_bus: Some(bus),
                env: Arc::new(MapEnv::from_iter([(KEY_ENV, "test-key")])),
                catalog: ModelCatalog::new(),
                factory: FactoryOptions::default(),
            },
        )
        .expect("direct routing composition succeeds"),
        config.agents.clone(),
    );
    let ModelIdentity::Routed { selected, .. } = composed.model_identity else {
        panic!("configured source must report routed identity");
    };

    for role in [
        Role::Orchestrator,
        Role::Explorer,
        Role::Worker,
        Role::Reviewer,
    ] {
        assert_eq!(
            selected.get(&role.name().to_lowercase()),
            Some(&direct.selected_model(role, None))
        );
    }
}

// Given: git リポジトリではないディレクトリを project root にした workspace seam
// When: compose_runtime が返した runtime で Worker を起動する
// Then: seam は canonical な root を受け付け、run はその root で正常終了する (ADR 0031)
#[tokio::test]
async fn fixed_composition_accepts_a_non_git_project_root() {
    let directory = tempfile::tempdir().expect("plain directory");
    let root = directory.path().canonicalize().expect("canonical root");
    let config = Config::default();
    let bus = Arc::new(EventBus::new(64));
    let seam = WorkspaceSeam::with_factory(root.clone(), Arc::new(DirectSandboxFactory))
        .expect("a non-git directory is a valid project root");
    assert_eq!(seam.project_root(), root.as_path());
    let composed = compose_runtime(composition(
        &config,
        bus,
        ModelSource::Fixed(Arc::new(ScriptedModel::new([Ok(text_response(
            "done",
            providers::FinishReason::Stop,
        ))]))),
        Some(seam),
    ))
    .expect("fixed model composes with workspace");

    let run = composed.runtime.delegate_background(
        Role::Worker,
        "work".to_string(),
        RunConfig::default(),
    );

    assert_eq!(composed.runtime.wait(run).await, Ok(AgentRunPhase::Done));
    assert_eq!(
        composed
            .runtime
            .inspect_agent(run)
            .expect("inspection")
            .workspace
            .expect("workspace inspection")
            .active_root,
        Some(root)
    );
    assert!(!directory.path().join(".evorch").exists());
}

// Given: 存在しない project root / When: workspace seam を作る / Then: fail-closed でエラーを返す
#[test]
fn workspace_seam_rejects_a_missing_project_root() {
    let directory = tempfile::tempdir().expect("parent directory");
    let missing = directory.path().join("missing");

    assert!(matches!(
        WorkspaceSeam::with_factory(missing, Arc::new(DirectSandboxFactory)),
        Err(runtime::RuntimeError::Workspace { .. })
    ));
}
