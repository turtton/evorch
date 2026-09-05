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
    AgentModel, AgentRunPhase, ModelIdentity, ModelSource, Role, RoutedModel, RunConfig,
    RuntimeComposition, WorkspaceMode, WorkspaceSeam, compose_runtime,
};
use sandbox::DirectSandbox;
use sandbox::credential::{CredentialStore, FileCredentialStore};
use tokio::sync::Notify;
use tokio::time::{Duration, timeout};
use tools::ToolExecutor;

use support::{ScriptedModel, drain_events, init_git_repo, text_response};

const PROFILE: &str = "local";
const MODEL: &str = "local-model";
const KEY_ENV: &str = "EVORCH_TEST_KEY_COMPOSITION_ROOT";

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
                models: vec![MODEL.to_string()],
                default_model: MODEL.to_string(),
            },
        )]),
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
// Then: provider 構成を要求せず Fixed identity の空 runtime が得られる
#[tokio::test]
async fn fixed_source_ignores_missing_providers() {
    let config = Config::default();
    let bus = Arc::new(EventBus::new(32));
    let composed = compose_runtime(composition(
        &config,
        bus,
        ModelSource::Fixed(Arc::new(ScriptedModel::new([Ok(text_response(
            "fixed done",
            providers::FinishReason::Stop,
        ))]))),
        None,
    ))
    .expect("fixed model composes without providers");

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
                catalog: ModelCatalog::builtin(),
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
            Some(&direct.selected_model(role))
        );
    }
}

// Given: production workspace seam と完了を gate した固定モデル
// When: compose_runtime が返した runtime で isolated Worker を起動する
// Then: モデル呼び出し中に run 専用 worktree が存在し、run は正常終了する
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fixed_composition_uses_workspace_seam_for_isolated_run() {
    let (_directory, repo) = init_git_repo();
    let config = Config::default();
    let bus = Arc::new(EventBus::new(64));
    let gate = Arc::new(Notify::new());
    let model = Arc::new(ScriptedModel::gated(
        [Ok(text_response(
            "isolated done",
            providers::FinishReason::Stop,
        ))],
        Arc::clone(&gate),
    ));
    let seam = WorkspaceSeam::production(repo.clone()).expect("workspace seam");
    let composed = compose_runtime(composition(
        &config,
        bus,
        ModelSource::Fixed(model),
        Some(seam),
    ))
    .expect("fixed model composes with workspace");

    let run = composed.runtime.delegate_background(
        Role::Worker,
        "isolated".to_string(),
        RunConfig {
            workspace_mode: WorkspaceMode::Isolated,
            ..RunConfig::default()
        },
    );
    let worktree = repo.join(".evorch/worktrees").join(run.to_string());
    timeout(Duration::from_secs(5), async {
        while !worktree.exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("isolated worktree appears");
    gate.notify_one();

    assert_eq!(composed.runtime.wait(run).await, Ok(AgentRunPhase::Done));
}

// Given: workspace seam を持たない固定モデル composition
// When: composed runtime で isolated Worker を起動する
// Then: workspace context 必須エラーをイベントで公開して Error 終了する
#[tokio::test]
async fn fixed_composition_without_workspace_rejects_isolated_run() {
    let config = Config::default();
    let bus = Arc::new(EventBus::new(64));
    let mut events = bus.subscribe();
    let composed = compose_runtime(composition(
        &config,
        bus,
        ModelSource::Fixed(Arc::new(ScriptedModel::new([Ok(text_response(
            "unused",
            providers::FinishReason::Stop,
        ))]))),
        None,
    ))
    .expect("fixed model composes without workspace");

    let run = composed.runtime.delegate_background(
        Role::Worker,
        "isolated".to_string(),
        RunConfig {
            workspace_mode: WorkspaceMode::Isolated,
            ..RunConfig::default()
        },
    );

    assert_eq!(composed.runtime.wait(run).await, Ok(AgentRunPhase::Error));
    let drained = drain_events(&mut events).await;
    assert!(drained.iter().any(|event| matches!(
        &event.kind,
        event_bus::EventKind::Lifecycle(event_bus::LifecycleEvent::AgentRunStateChanged {
            to: AgentRunPhase::Error,
            reason: Some(reason),
            ..
        }) if reason == "workspace isolation requires workspace context"
    )));
}
