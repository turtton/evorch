//! A D3 replacement must remain effective through later compaction and failed resume.

use std::sync::Arc;

use config::{Config, EditToolConfig, LoadOptions};
use event_bus::{
    AgentRunPhase, CompactionEvent, CompactionReason, EventBus, EventKind, EventReceiver,
    LifecycleEvent,
};
use mock_openai::{ScriptedResponse, StreamingMockOpenAi};
use runtime::{AgentRuntime, ModelSource, Role, RunConfig, RuntimeComposition, compose_runtime};
use sandbox::credential::FileCredentialStore;
use serde_json::Value;
use tools::ToolExecutor;

use super::{PATCH, editing_tools, patch_call, read_call, text, workspace};
use crate::{BulkRead, KEY_ENV, MODEL, Wire, contract_prompts, through_phase};

struct Fixture {
    runtime: AgentRuntime,
    receiver: EventReceiver,
    next: StreamingMockOpenAi,
    _head: StreamingMockOpenAi,
    _workspace: tempfile::TempDir,
    _directory: tempfile::TempDir,
    _store: crate::support::ReviewStorage,
}

fn fixture(script: Vec<ScriptedResponse>, target_window: u64, max_summary_bytes: u64) -> Fixture {
    let (workspace, root) = workspace();
    let head = StreamingMockOpenAi::spawn_with_prompt_cache_models(
        vec![
            text(MODEL, "prior turn"),
            patch_call(Wire::ChatCompletions, "old-editing-call", &PATCH),
        ],
        vec![MODEL.into()],
    );
    let next = StreamingMockOpenAi::spawn_with_prompt_cache_models(script, vec![MODEL.into()]);
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(directory.path().join(config::PROJECT_CONFIG_DIR)).unwrap();
    std::fs::write(
        config::project_main_config_path(directory.path()),
        format!(
            r#"
version = 3
[providers.head]
type = "openai-compatible"
base_url = "{head_url}"
api_key_env = "{KEY_ENV}"
models = ["{MODEL}"]
default_model = "{MODEL}"
[providers.next]
type = "openai-compatible"
base_url = "{next_url}"
api_key_env = "{KEY_ENV}"
models = ["{MODEL}"]
default_model = "{MODEL}"
[[routing.routes.head]]
profile = "head"
[[routing.routes.next]]
profile = "next"
[agents.worker]
models = [{{ route = "head" }}, {{ route = "next" }}]
[compaction]
context_window_tokens = 1000000
threshold = 0.5
keep_recent_tokens = 1
max_summary_bytes = {max_summary_bytes}
summarizer = "model"
[compaction.model_overrides]
"next/{MODEL}" = {target_window}
"#,
            head_url = head.base_url(),
            next_url = next.base_url()
        ),
    )
    .unwrap();
    let mut config = Config::load(&LoadOptions {
        project_dir: Some(directory.path().into()),
        user_config_dir: Some(directory.path().join(config::PROJECT_CONFIG_DIR)),
        read_env: false,
        ..Default::default()
    })
    .unwrap();
    config.providers.get_mut("head").unwrap().models[0].edit_tool =
        Some(EditToolConfig::ApplyPatch);
    config.providers.get_mut("next").unwrap().models[0].edit_tool = Some(EditToolConfig::Edit);
    let bus = Arc::new(EventBus::new(2048));
    let receiver = bus.subscribe();
    let mut executor = ToolExecutor::new(bus.clone());
    executor.register(Arc::new(BulkRead)).unwrap();
    for tool in editing_tools() {
        executor.register(tool).unwrap();
    }
    let store = Arc::new(FileCredentialStore::open(directory.path().join("credentials")).unwrap());
    let runtime = compose_runtime(RuntimeComposition {
        model_catalog_dir: Some(test_support::model_catalog_dir()),
        user_config_dir: Some(directory.path().join("empty-user-config")),
        config: &config,
        bus,
        executor: Arc::new(executor),
        credential_store: store,
        env: Arc::new(routing::MapEnv::from_iter([(KEY_ENV, "offline-key")])),
        model_source: ModelSource::Configured,
        workspace: None,
    })
    .unwrap()
    .runtime
    .with_sequential_run_ids()
    .with_system_prompts(contract_prompts())
    .with_compaction(config.compaction.clone());
    runtime.set_project_root(root).unwrap();
    let review_store = crate::support::ReviewStorage::new();
    review_store.attach(&runtime);
    Fixture {
        runtime,
        receiver,
        next,
        _head: head,
        _workspace: workspace,
        _directory: directory,
        _store: review_store,
    }
}

fn requests(mock: &StreamingMockOpenAi) -> Vec<Value> {
    mock.recorded_requests()
        .into_iter()
        .filter(|request| request.method == "POST" && request.path == Wire::ChatCompletions.path())
        .map(|request| request.body)
        .collect()
}

fn assert_no_old_tool_items(request: &Value) {
    for message in request["messages"].as_array().unwrap() {
        assert_ne!(message["tool_call_id"], "old-editing-call");
        assert!(
            !message["tool_calls"]
                .as_array()
                .is_some_and(|calls| calls.iter().any(|call| call["id"] == "old-editing-call")),
            "old editing call must not reappear"
        );
    }
}

async fn next_boundary(
    fixture: &mut Fixture,
    run: runtime::RunId,
) -> (AgentRunPhase, Vec<event_bus::Event>) {
    let mut events = Vec::new();
    loop {
        let event = fixture.receiver.recv().await.unwrap();
        let phase = match &event.kind {
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to, .. })
                if run_id == &run.to_string()
                    && matches!(
                        to,
                        AgentRunPhase::Waiting | AgentRunPhase::Done | AgentRunPhase::Error
                    ) =>
            {
                Some(*to)
            }
            _ => None,
        };
        events.push(event);
        if let Some(phase) = phase {
            return (phase, events);
        }
    }
}

async fn begin(fixture: &mut Fixture) -> runtime::RunId {
    let run = fixture.runtime.delegate_background(
        Role::Worker,
        "retain the goal".into(),
        RunConfig {
            interactive: true,
            keep_alive: true,
            ..Default::default()
        },
    );
    through_phase(&mut fixture.receiver, run, AgentRunPhase::Waiting).await;
    fixture
        .runtime
        .send_message(run, "Edit notes now".into())
        .unwrap();
    run
}

#[tokio::test]
async fn window_compaction_immediately_after_d3_never_reveals_old_calls() {
    let mut fixture = fixture(
        vec![
            text(MODEL, "D3 summary"),
            text(MODEL, "window summary"),
            text(MODEL, "done"),
        ],
        6_000,
        256,
    );
    let run = begin(&mut fixture).await;
    let (phase, events) = next_boundary(&mut fixture, run).await;
    assert_eq!(phase, AgentRunPhase::Waiting);
    let reasons: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Compaction(CompactionEvent::Compacted { reason, .. }) => Some(*reason),
            _ => None,
        })
        .collect();
    assert_eq!(
        reasons,
        [CompactionReason::ModelSwitch, CompactionReason::ModelSwitch]
    );
    let requests = requests(&fixture.next);
    assert_eq!(requests.len(), 3);
    for request in &requests {
        assert_no_old_tool_items(request);
    }
    fixture
        .runtime
        .stop(run, runtime::StopScope::SelfOnly)
        .unwrap();
    assert_eq!(
        fixture.runtime.wait(run).await.unwrap(),
        AgentRunPhase::Stopped
    );
}

#[tokio::test]
async fn oversized_d3_summary_is_recompacted_to_fit_the_target_window() {
    let mut fixture = fixture(
        vec![
            text(MODEL, &"summarizable evidence ".repeat(600)),
            text(MODEL, "small window summary"),
            text(MODEL, "done"),
        ],
        6_000,
        16_000,
    );
    let run = begin(&mut fixture).await;
    let (phase, _) = next_boundary(&mut fixture, run).await;
    assert_eq!(phase, AgentRunPhase::Waiting);
    let requests = requests(&fixture.next);
    assert_eq!(requests.len(), 3);
    for request in &requests {
        assert_no_old_tool_items(request);
    }
    fixture
        .runtime
        .stop(run, runtime::StopScope::SelfOnly)
        .unwrap();
    assert_eq!(
        fixture.runtime.wait(run).await.unwrap(),
        AgentRunPhase::Stopped
    );
}

#[tokio::test]
async fn later_manual_compaction_respects_the_d3_replacement() {
    later_compaction(false).await;
}

#[tokio::test]
async fn later_automatic_compaction_respects_the_d3_replacement() {
    later_compaction(true).await;
}

async fn later_compaction(automatic: bool) {
    let mut fixture = fixture(
        vec![
            text(MODEL, "D3 summary"),
            text(
                MODEL,
                &"old answer ".repeat(if automatic { 20_000 } else { 1 }),
            ),
            text(MODEL, "later summary"),
            read_call(MODEL, 0),
            text(MODEL, "done"),
        ],
        if automatic { 60_000 } else { 1_000_000 },
        256,
    );
    let run = begin(&mut fixture).await;
    through_phase(&mut fixture.receiver, run, AgentRunPhase::Waiting).await;
    if !automatic {
        fixture.runtime.compact(run).unwrap();
    }
    fixture
        .runtime
        .send_message(run, "Continue after D3".into())
        .unwrap();
    let (phase, events) = next_boundary(&mut fixture, run).await;
    assert_eq!(phase, AgentRunPhase::Waiting);
    let reasons: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Compaction(CompactionEvent::Compacted { reason, .. }) => Some(*reason),
            _ => None,
        })
        .collect();
    assert_eq!(
        reasons,
        [if automatic {
            CompactionReason::Automatic
        } else {
            CompactionReason::Manual
        }]
    );
    let requests = requests(&fixture.next);
    assert_eq!(requests.len(), 5);
    for request in &requests {
        assert_no_old_tool_items(request);
    }
    fixture
        .runtime
        .stop(run, runtime::StopScope::SelfOnly)
        .unwrap();
    assert_eq!(
        fixture.runtime.wait(run).await.unwrap(),
        AgentRunPhase::Stopped
    );
}

#[tokio::test]
async fn failed_d3_preparation_cannot_be_bypassed_by_resuming_the_same_run() {
    let mut fixture = fixture(
        vec![
            text(MODEL, "summary discarded"),
            text(MODEL, "retry summary discarded"),
        ],
        1_000_000,
        0,
    );
    let run = begin(&mut fixture).await;
    let (phase, _) = next_boundary(&mut fixture, run).await;
    assert_eq!(phase, AgentRunPhase::Error);
    assert_eq!(
        fixture.runtime.wait(run).await.unwrap(),
        AgentRunPhase::Error
    );
    fixture
        .runtime
        .resume_chat(run, RunConfig::default())
        .unwrap();
    let (phase, _) = next_boundary(&mut fixture, run).await;
    assert_eq!(phase, AgentRunPhase::Error);
    assert_eq!(
        fixture.runtime.wait(run).await.unwrap(),
        AgentRunPhase::Error
    );
    let requests = requests(&fixture.next);
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert!(
            request["tools"].as_array().is_none_or(Vec::is_empty),
            "failed preparation must never send an agent request: {request}"
        );
        assert_no_old_tool_items(request);
    }
}
