//! Offline cost-regression contract through runtime, routing, HTTP/SSE and usage.
//! Mock tokens are synthetic bytes, not a prediction of production billing.
use std::sync::Arc;
use std::time::Duration;

use config::{Config, LoadOptions};
use event_bus::{
    AgentRunPhase, CompactionEvent, CompactionReason, Event, EventBus, EventKind, EventReceiver,
    LifecycleEvent, ProviderEvent, UsageAggregator,
};
use mock_openai::cache_contract::{
    CacheProtocol, assert_append_only, assert_compaction_transition,
};
use mock_openai::{ScriptedResponse, StreamingMockOpenAi};
use routing::MapEnv;
use runtime::{
    AgentRuntime, DelegateImage, ModelSource, Role, RunConfig, RunId, RuntimeComposition,
    SystemPromptCatalog, compose_runtime,
};
use sandbox::credential::FileCredentialStore;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tools::{Permissions, Tool, ToolError, ToolExecutor, ToolResult};

const MODEL: &str = "mock-model";
const KEY_ENV: &str = "EVORCH_CACHE_E2E_KEY";

struct BulkRead;
#[async_trait::async_trait]
impl Tool for BulkRead {
    fn name(&self) -> &'static str {
        "read"
    }
    fn schema(&self) -> Value {
        json!({"type":"object", "required":["index"], "properties":{"index":{"type":"integer"}}})
    }
    fn permissions(&self) -> Permissions {
        Permissions::read_only()
    }
    async fn execute(&self, input: Value) -> Result<ToolResult, ToolError> {
        let index = input["index"].as_u64().unwrap();
        let output =
            format!("result-{index}: 日本語とASCII\n").repeat(if index == 1 { 2000 } else { 80 });
        Ok(if index == 2 {
            ToolResult::error(output)
        } else {
            ToolResult::success(output)
        })
    }
}

struct GatedRead {
    started: Notify,
    release: Notify,
    child_started: Notify,
}

#[async_trait::async_trait]
impl Tool for GatedRead {
    fn name(&self) -> &'static str {
        BulkRead.name()
    }
    fn schema(&self) -> Value {
        BulkRead.schema()
    }
    fn permissions(&self) -> Permissions {
        BulkRead.permissions()
    }
    async fn execute(&self, input: Value) -> Result<ToolResult, ToolError> {
        if input["index"] == 0 {
            self.started.notify_one();
            self.release.notified().await;
        } else if input["index"] == 1 {
            self.child_started.notify_one();
            std::future::pending::<()>().await;
        }
        BulkRead.execute(input).await
    }
}

struct Harness {
    _directory: tempfile::TempDir,
    runtime: AgentRuntime,
    receiver: EventReceiver,
    mock: StreamingMockOpenAi,
}

fn harness(script: Vec<ScriptedResponse>, window: u64) -> Harness {
    harness_with_read(script, window, Arc::new(BulkRead))
}

fn harness_with_read(script: Vec<ScriptedResponse>, window: u64, read: Arc<dyn Tool>) -> Harness {
    harness_with_tools(script, window, read, Vec::new())
}

fn harness_with_tools(
    script: Vec<ScriptedResponse>,
    window: u64,
    read: Arc<dyn Tool>,
    extra_tools: Vec<Arc<dyn Tool>>,
) -> Harness {
    let directory = tempfile::tempdir().unwrap();
    let mock = StreamingMockOpenAi::spawn_with_prompt_cache(script);
    std::fs::create_dir_all(directory.path().join(config::PROJECT_CONFIG_DIR))
        .expect("config directory");
    std::fs::write(
        config::project_main_config_path(directory.path()),
        format!(
            r#"
[providers.local]
type = "openai-compatible"
base_url = "{}"
api_key_env = "{KEY_ENV}"
models = ["{MODEL}"]
default_model = "{MODEL}"
[[routing.routes.orchestrator]]
profile = "local"
[[routing.routes.worker]]
profile = "local"
[compaction]
context_window_tokens = {window}
threshold = 0.5
keep_recent_tokens = 1
max_summary_bytes = 256
summarizer = "structural"
"#,
            mock.base_url()
        ),
    )
    .unwrap();
    let config = Config::load(&LoadOptions {
        project_dir: Some(directory.path().into()),
        user_config_dir: Some(directory.path().join("empty-user-config")),
        read_env: false,
        ..Default::default()
    })
    .unwrap();
    let bus = Arc::new(EventBus::new(1024));
    let receiver = bus.subscribe();
    let mut executor = ToolExecutor::new(bus.clone());
    executor.register(read).unwrap();
    for tool in extra_tools {
        executor.register(tool).unwrap();
    }
    let mut prompts = SystemPromptCatalog::builder().category_overlay(
        "conversation",
        include_str!("../../config/assets/presets/category-conversation.md"),
    );
    for role in [
        Role::Orchestrator,
        Role::Explorer,
        Role::Worker,
        Role::Reviewer,
    ] {
        prompts = prompts.role_baseline(role, "Stable cache contract instructions");
    }
    for family in [
        "family-claude",
        "family-openai-reasoning",
        "family-gpt5",
        "family-gemini",
        "family-kimi",
        "family-generic",
    ] {
        prompts = prompts.family_section(family, "Use tools and preserve context");
    }
    let runtime = compose_runtime(RuntimeComposition {
        user_config_dir: Some(directory.path().join("empty-user-config")),
        config: &config,
        bus,
        executor: Arc::new(executor),
        credential_store: Arc::new(
            FileCredentialStore::open(directory.path().join("credentials")).unwrap(),
        ),
        env: Arc::new(MapEnv::from_iter([(KEY_ENV, "offline-test-key")])),
        model_source: ModelSource::Configured,
        workspace: None,
    })
    .unwrap()
    .runtime
    .with_system_prompts(Arc::new(prompts.build().unwrap()))
    .with_compaction(config.compaction.clone());
    Harness {
        _directory: directory,
        runtime,
        receiver,
        mock,
    }
}

fn read_response(index: usize) -> ScriptedResponse {
    ScriptedResponse::tool_call(
        &format!("response-{index}"),
        MODEL,
        0,
        &format!("call-{index}"),
        "read",
        [json!({"index":index}).to_string()],
    )
}
fn text_response(text: &str) -> ScriptedResponse {
    ScriptedResponse::text_stream("reply", MODEL, [text])
}

async fn through_phase(
    receiver: &mut EventReceiver,
    run: RunId,
    phase: AgentRunPhase,
) -> Vec<Event> {
    tokio::time::timeout(Duration::from_secs(20), async {
        let mut events = Vec::new();
        loop {
            let event = receiver.recv().await.unwrap();
            let done = matches!(&event.kind,
                EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {run_id, to, ..})
                    if run_id == &run.to_string() && *to == phase);
            events.push(event);
            if done {
                return events;
            }
        }
    })
    .await
    .expect("runtime phase timeout")
}

fn verify_trace(harness: &Harness, run: RunId, events: &[Event], compactions: usize) {
    let requests = harness
        .mock
        .recorded_requests()
        .into_iter()
        .filter(|request| request.path == "/v1/chat/completions")
        .map(|request| request.body)
        .collect::<Vec<_>>();
    let mut starts = Vec::new();
    let mut completed = std::collections::HashMap::new();
    let mut pending_checkpoint = None;
    let mut checkpoint_count = 0;
    let mut aggregator = UsageAggregator::new();
    for event in events {
        match &event.kind {
            EventKind::Compaction(CompactionEvent::Compacted {
                run_id,
                checkpoint_id,
                ..
            }) if run_id == &run.to_string() => {
                assert!(pending_checkpoint.replace(checkpoint_id.clone()).is_none());
                checkpoint_count += 1;
            }
            EventKind::Provider(ProviderEvent::RequestStarted {
                request_id,
                run_id: Some(id),
                ..
            }) if id == &run.to_string() => {
                starts.push((request_id.clone(), pending_checkpoint.take()));
            }
            EventKind::Provider(ProviderEvent::RequestCompleted {
                request_id,
                input_tokens,
                cache_read_tokens,
                run_id: Some(id),
                ..
            }) if id == &run.to_string() => {
                assert!(
                    completed
                        .insert(request_id.clone(), (*input_tokens, *cache_read_tokens))
                        .is_none()
                );
            }
            EventKind::Usage(usage) => aggregator.record(usage, &event.meta),
            EventKind::Diagnostic(diagnostic) => {
                assert_ne!(diagnostic.code, "CacheRegression", "{diagnostic:?}")
            }
            _ => {}
        }
    }
    assert_eq!(checkpoint_count, compactions);
    assert!(
        pending_checkpoint.is_none(),
        "compaction must be followed by a request"
    );
    assert_eq!(requests.len(), starts.len());
    assert_eq!(requests.len(), completed.len());
    assert!(requests.len() >= 3);
    assert_eq!(harness.mock.remaining_scripts(), 0);
    let mut total_input = 0;
    let mut total_cached = 0;
    for (index, (request_id, checkpoint)) in starts.iter().enumerate() {
        let (input, cached) = completed[request_id];
        assert!(input > 0 && cached <= input);
        total_input += input;
        total_cached += cached;
        if index == 0 {
            assert!(checkpoint.is_none());
            assert_eq!(cached, 0, "first request must be cold");
            continue;
        }
        let previous_input = completed[&starts[index - 1].0].0;
        if let Some(checkpoint) = checkpoint {
            // Only the successful event for this run, consumed at this exact next
            // request, permits replacing history. Static instructions/tools stay fixed.
            assert_compaction_transition(
                CacheProtocol::OpenAi,
                &requests[index - 1],
                &requests[index],
                checkpoint,
            )
            .unwrap();
            assert!(
                cached < previous_input,
                "compaction fixture must actually invalidate part of the old prefix"
            );
        } else {
            assert_append_only(
                CacheProtocol::OpenAi,
                &requests[index - 1],
                &requests[index],
            )
            .unwrap_or_else(|error| panic!("request {index} destroyed cached input: {error}"));
            assert!(
                cached >= previous_input,
                "request {index}: cached={cached}, previously processed input={previous_input}"
            );
        }
    }
    let buckets = aggregator.drain();
    assert_eq!(
        buckets.iter().map(|b| b.request_count).sum::<u64>(),
        requests.len() as u64
    );
    assert_eq!(
        buckets.iter().map(|b| b.input_tokens).sum::<u64>(),
        total_input
    );
    assert_eq!(
        buckets.iter().map(|b| b.cache_read_tokens).sum::<u64>(),
        total_cached
    );
    assert!(total_cached > 0);
}

#[tokio::test]
async fn interrupted_tool_recovery_appends_error_context_and_preserves_the_wire_prefix() {
    interrupted_tool_recovery("Explain the interrupted result", false).await;
}

#[tokio::test]
async fn continue_after_stop_or_error_preserves_the_wire_prefix() {
    for stopped in [false, true] {
        interrupted_tool_recovery(AgentRuntime::CHAT_CONTINUE_PROMPT, stopped).await;
    }
}

async fn interrupted_tool_recovery(prompt: &str, stopped: bool) {
    let read = Arc::new(GatedRead {
        started: Notify::new(),
        release: Notify::new(),
        child_started: Notify::new(),
    });
    let mut harness = harness_with_read(
        vec![
            read_response(0),
            read_response(2),
            text_response("continued"),
        ],
        1_000_000,
        read.clone(),
    );
    let config = storage::StorageConfig {
        db_path: harness._directory.path().join("history.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    harness.runtime = harness
        .runtime
        .with_run_store(runtime::RunStore::open(&config, storage.handle()).unwrap());
    let run = harness.runtime.delegate_background(
        Role::Worker,
        "Keep this conversation".into(),
        RunConfig::default(),
    );
    tokio::time::timeout(Duration::from_secs(20), read.started.notified())
        .await
        .unwrap();
    if stopped {
        harness
            .runtime
            .stop(run, runtime::StopScope::SelfOnly)
            .unwrap();
    } else {
        harness.runtime.cancel(run).unwrap();
    }
    let phase = if stopped {
        AgentRunPhase::Stopped
    } else {
        AgentRunPhase::Error
    };
    let mut events = through_phase(&mut harness.receiver, run, phase).await;
    harness.runtime.wait(run).await.unwrap();
    harness
        .runtime
        .continue_goal(run, prompt.into(), RunConfig::default())
        .unwrap();
    events.extend(through_phase(&mut harness.receiver, run, AgentRunPhase::Waiting).await);
    harness.runtime.cancel(run).unwrap();
    harness.runtime.wait(run).await.unwrap();
    verify_trace(&harness, run, &events, 0);
    let requests = harness.mock.recorded_requests();
    let request = &requests
        .iter()
        .filter(|r| r.path == "/v1/chat/completions")
        .nth(1)
        .unwrap()
        .body;
    let text = request.to_string();
    assert!(text.contains("ToolExecutionOutcomeUnknown"));
    assert!(text.contains(prompt));
    if !stopped {
        assert!(text.contains("cancelled"));
    }
}

#[tokio::test]
async fn ordinary_tool_turns_reuse_all_previous_wire_input() {
    // Includes returned artifacts, errors, multibyte text and the former eight-result boundary.
    let script = (0..12)
        .map(read_response)
        .chain([text_response("done")])
        .collect();
    let mut harness = harness(script, 1_000_000);
    let run = harness.runtime.delegate_background(
        Role::Worker,
        "Read twelve results".into(),
        RunConfig::default(),
    );
    let events = through_phase(&mut harness.receiver, run, AgentRunPhase::Done).await;
    assert_eq!(
        harness.runtime.wait(run).await.unwrap(),
        AgentRunPhase::Done
    );
    verify_trace(&harness, run, &events, 0);
}

#[tokio::test]
async fn conversation_chat_preserves_overlay_web_schemas_and_wire_prefix_on_followup() {
    // Only expose the real Web schemas; this offline scenario never invokes the network.
    let mut harness = harness_with_tools(
        vec![
            read_response(0),
            text_response("waiting"),
            read_response(2),
            text_response("done"),
        ],
        1_000_000,
        Arc::new(BulkRead),
        vec![
            Arc::new(tools::WebSearch::keyless_default().unwrap()),
            Arc::new(tools::WebFetch::new().unwrap()),
        ],
    );
    let run = harness
        .runtime
        .delegate_chat(
            "conversation-cache",
            Role::Worker,
            "Read and discuss the results".into(),
            RunConfig {
                conversation: true,
                category: Some("conversation".into()),
                interactive: true,
                ..Default::default()
            },
        )
        .unwrap();
    let mut events = through_phase(&mut harness.receiver, run, AgentRunPhase::Waiting).await;
    harness
        .runtime
        .send_message(run, "Continue the discussion".into())
        .unwrap();
    events.extend(through_phase(&mut harness.receiver, run, AgentRunPhase::Done).await);
    assert_eq!(
        harness.runtime.wait(run).await.unwrap(),
        AgentRunPhase::Done
    );
    verify_trace(&harness, run, &events, 0);
    for request in harness
        .mock
        .recorded_requests()
        .iter()
        .filter(|r| r.path == "/v1/chat/completions")
    {
        let tools = request.body["tools"].as_array().unwrap();
        for name in ["read", "web_search", "web_fetch"] {
            assert!(tools.iter().any(|tool| tool["function"]["name"] == name));
        }
        assert!(
            !tools
                .iter()
                .any(|tool| tool["function"]["name"] == "delegate")
        );
        let system = request.body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "system")
            .unwrap();
        assert!(
            system["content"]
                .as_str()
                .unwrap()
                .contains("# Conversation カテゴリオーバーレイ")
        );
    }
}

// Changing execution scope appends observations; it never changes tools or rewrites errors.
#[tokio::test]
async fn shell_scope_recovery_keeps_policy_and_failed_observations_in_the_wire_prefix() {
    use tools::tools::shell_escalation::{EscalationDecision, ShellEscalationGate};

    struct ApproveHost;
    #[async_trait::async_trait]
    impl ShellEscalationGate for ApproveHost {
        async fn decide(
            &self,
            _ctx: &tools::ToolExecutionContext,
            _command: &str,
            _justification: &str,
        ) -> EscalationDecision {
            EscalationDecision::Approve
        }
    }

    let shell = Arc::new(tools::Shell::new(Arc::new(
        sandbox::DirectSandbox::new_unchecked(),
    )));
    let commands = [
        json!({"command":"printf 'fixture authentication unavailable\\n'; exit 4"}),
        json!({"command":"true", "sandbox_access":"network", "justification":"check connectivity"}),
        json!({"command":"printf 'fixture host check complete\\n'", "sandbox_access":"unsandboxed", "justification":"check host environment for the requested task"}),
    ];
    let script = commands
        .into_iter()
        .enumerate()
        .map(|(index, args)| {
            ScriptedResponse::tool_call(
                &format!("shell-response-{index}"),
                MODEL,
                0,
                &format!("shell-call-{index}"),
                "shell",
                [args.to_string()],
            )
        })
        .chain([text_response("done")])
        .collect();
    let mut harness =
        harness_with_tools(script, 1_000_000, Arc::new(BulkRead), vec![shell.clone()]);
    // Replace the model reviewer only for this offline execution/cache fixture.
    shell.set_shell_escalation(Arc::new(ApproveHost), sandbox::composition::unsandboxed());
    let run = harness.runtime.delegate_background(
        Role::Worker,
        "Diagnose the command in its execution environment".into(),
        RunConfig::default(),
    );
    let events = through_phase(&mut harness.receiver, run, AgentRunPhase::Done).await;
    assert_eq!(
        harness.runtime.wait(run).await.unwrap(),
        AgentRunPhase::Done
    );
    verify_trace(&harness, run, &events, 0);
    let requests = harness.mock.recorded_requests();
    let first = &requests
        .iter()
        .find(|r| r.path == "/v1/chat/completions")
        .unwrap()
        .body;
    let shell_spec = first["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["function"]["name"] == "shell")
        .unwrap();
    assert!(
        shell_spec["function"]["description"]
            .as_str()
            .unwrap()
            .contains("private HOME")
    );
    let last = &requests
        .iter()
        .rfind(|r| r.path == "/v1/chat/completions")
        .unwrap()
        .body;
    let tool_output = |id: &str| {
        last["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "tool" && message["tool_call_id"] == id)
            .unwrap()["content"]
            .as_str()
            .unwrap()
    };
    let isolated = tool_output("shell-call-0");
    assert!(isolated.contains("exit_code: 4"));
    assert!(isolated.contains("fixture authentication unavailable"));
    assert!(tool_output("shell-call-1").contains("network-only shell access is unavailable"));
    let host = tool_output("shell-call-2");
    assert!(host.contains("exit_code: 0"));
    assert!(host.contains("fixture host check complete"));
}

async fn compaction_restarts_cache(reason: CompactionReason) {
    let automatic = reason == CompactionReason::Automatic;
    let old_reply =
        "old analysis that can be summarized ".repeat(if automatic { 6000 } else { 100 });
    let mut harness = harness(
        vec![
            read_response(0),
            text_response(&old_reply),
            read_response(1),
            read_response(2),
            text_response("done"),
        ],
        if automatic { 64_000 } else { 1_000_000 },
    );
    let run = harness.runtime.delegate_background(
        Role::Worker,
        "Preserve this original goal".into(),
        RunConfig {
            interactive: true,
            ..Default::default()
        },
    );
    let mut events = through_phase(&mut harness.receiver, run, AgentRunPhase::Waiting).await;
    if !automatic {
        harness.runtime.compact(run).unwrap();
    }
    harness
        .runtime
        .send_message(run, "Continue after this boundary".into())
        .unwrap();
    events.extend(through_phase(&mut harness.receiver, run, AgentRunPhase::Done).await);
    assert_eq!(
        harness.runtime.wait(run).await.unwrap(),
        AgentRunPhase::Done
    );
    assert!(events.iter().any(|event| matches!(&event.kind,
        EventKind::Compaction(CompactionEvent::Compacted {reason: actual, ..}) if *actual == reason)));
    verify_trace(&harness, run, &events, 1);
}

#[tokio::test]
async fn manual_compaction_can_replace_history_then_warms_a_new_prefix() {
    compaction_restarts_cache(CompactionReason::Manual).await;
}

#[tokio::test]
async fn automatic_compaction_can_replace_history_then_warms_a_new_prefix() {
    compaction_restarts_cache(CompactionReason::Automatic).await;
}

#[tokio::test]
async fn wait_interrupted_by_ui_text_and_images_reuses_the_wire_prefix() {
    let read = Arc::new(GatedRead {
        started: Notify::new(),
        release: Notify::new(),
        child_started: Notify::new(),
    });
    let mut harness = harness_with_read(
        vec![
            read_response(0),
            read_response(1),
            ScriptedResponse::tool_call(
                "wait",
                MODEL,
                0,
                "wait-call",
                "wait",
                [json!({"run_id":"run-2"}).to_string()],
            ),
            ScriptedResponse::tool_call(
                "finish",
                MODEL,
                0,
                "finish-call",
                "finish",
                [json!({"result":"done"}).to_string()],
            ),
        ],
        1_000_000,
        read.clone(),
    );
    let prompt = "Handle follow-up input";
    let run = harness.runtime.delegate_background(
        Role::Orchestrator,
        prompt.into(),
        RunConfig::default(),
    );
    tokio::time::timeout(Duration::from_secs(20), read.started.notified())
        .await
        .unwrap();
    let child = harness
        .runtime
        .delegate_background_as_child(run, Role::Worker, "Blocked child", RunConfig::default())
        .unwrap();
    tokio::time::timeout(Duration::from_secs(20), read.child_started.notified())
        .await
        .unwrap();
    read.release.notify_one();
    let mut events = through_phase(&mut harness.receiver, run, AgentRunPhase::Waiting).await;
    harness
        .runtime
        .send_message_with_images(
            run,
            "Inspect the attached image".into(),
            vec![DelegateImage {
                media_type: "image/png".into(),
                data: "aW1hZ2U=".into(),
            }],
        )
        .unwrap();
    events.extend(through_phase(&mut harness.receiver, run, AgentRunPhase::Done).await);
    assert_eq!(
        harness.runtime.wait(run).await.unwrap(),
        AgentRunPhase::Done
    );
    harness.runtime.cancel(child).unwrap();
    harness.runtime.wait(child).await.unwrap();
    assert_eq!(harness.mock.remaining_scripts(), 0);
    let requests = harness
        .mock
        .recorded_requests()
        .into_iter()
        .filter(|request| request.path == "/v1/chat/completions")
        .filter(|request| {
            request.body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| message["role"] == "user" && message["content"] == prompt)
        })
        .map(|request| request.body)
        .collect::<Vec<_>>();
    assert_eq!(requests.len(), 3);
    let usage = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Provider(ProviderEvent::RequestCompleted {
                run_id: Some(id),
                input_tokens,
                cache_read_tokens,
                ..
            }) if id == &run.to_string() => Some((*input_tokens, *cache_read_tokens)),
            EventKind::Diagnostic(diagnostic) => {
                assert_ne!(diagnostic.code, "CacheRegression", "{diagnostic:?}");
                None
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(usage.len(), requests.len());
    for (wire, tokens) in requests.windows(2).zip(usage.windows(2)) {
        assert_append_only(CacheProtocol::OpenAi, &wire[0], &wire[1]).unwrap();
        assert!(
            tokens[1].1 >= tokens[0].0,
            "previous wire input must remain cached"
        );
    }
    let messages = requests[2]["messages"].as_array().unwrap();
    let input_index = messages
        .iter()
        .position(|message| {
            message["content"].as_array().is_some_and(|blocks| {
                blocks
                    .iter()
                    .any(|block| block["text"] == "Inspect the attached image")
            })
        })
        .expect("next wire request includes user input");
    assert_eq!(messages[input_index]["role"], "user");
    assert!(
        messages[input_index]["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|block| block["image_url"]["url"] == "data:image/png;base64,aW1hZ2U=")
    );
    assert_eq!(messages[input_index - 1]["role"], "tool");
    let result: Value =
        serde_json::from_str(messages[input_index - 1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(result["user_input_ready"], true);
    assert_eq!(result["runs"][0]["status"], "still_running");
}

#[tokio::test]
async fn queued_and_expedited_follow_ups_preserve_fifo_images_and_wire_prefix() {
    for expedite in [false, true] {
        let read = Arc::new(GatedRead {
            started: Notify::new(),
            release: Notify::new(),
            child_started: Notify::new(),
        });
        let mut script = vec![
            read_response(0),
            read_response(2),
            text_response("original answer"),
        ];
        if !expedite {
            script.push(text_response("follow-up answer"));
        }
        let mut harness = harness_with_read(script, 1_000_000, read.clone());
        let run = harness.runtime.delegate_background(
            Role::Worker,
            "Original task".into(),
            RunConfig::default(),
        );
        tokio::time::timeout(Duration::from_secs(20), read.started.notified())
            .await
            .unwrap();
        harness
            .runtime
            .send_message_with_images(
                run,
                "first queued follow-up".into(),
                vec![DelegateImage {
                    media_type: "image/png".into(),
                    data: "aW1hZ2U=".into(),
                }],
            )
            .unwrap();
        harness
            .runtime
            .send_message(run, "second queued follow-up".into())
            .unwrap();
        assert_eq!(
            harness.runtime.follow_up_status(run).unwrap(),
            runtime::FollowUpStatus {
                pending: 2,
                next_turn_requested: false,
                closed: false,
            }
        );
        // The in-flight tool must not be aborted, nor should delivery duplicate input.
        if expedite {
            harness.runtime.deliver_follow_ups_next_turn(run).unwrap();
            harness.runtime.deliver_follow_ups_next_turn(run).unwrap();
            assert!(
                harness
                    .runtime
                    .follow_up_status(run)
                    .unwrap()
                    .next_turn_requested
            );
        }
        assert_eq!(
            harness
                .mock
                .recorded_requests()
                .iter()
                .filter(|r| r.path == "/v1/chat/completions")
                .count(),
            1
        );
        read.release.notify_one();
        let events = through_phase(&mut harness.receiver, run, AgentRunPhase::Done).await;
        harness.runtime.wait(run).await.unwrap();
        assert_eq!(harness.runtime.follow_up_status(run).unwrap().pending, 0);
        assert!(
            !harness
                .runtime
                .follow_up_status(run)
                .unwrap()
                .next_turn_requested
        );
        verify_trace(&harness, run, &events, 0);
        let requests = harness.mock.recorded_requests();
        let bodies = requests
            .iter()
            .filter(|r| r.path == "/v1/chat/completions")
            .map(|r| &r.body)
            .collect::<Vec<_>>();
        assert_eq!(bodies.len(), if expedite { 3 } else { 4 });
        assert_eq!(
            bodies[1].to_string().contains("first queued follow-up"),
            expedite
        );
        assert_eq!(
            bodies[2].to_string().contains("first queued follow-up"),
            expedite
        );
        let messages = bodies.last().unwrap()["messages"].as_array().unwrap();
        let first = messages
            .iter()
            .position(|m| m.to_string().contains("first queued follow-up"))
            .unwrap();
        let second = messages
            .iter()
            .position(|m| m.to_string().contains("second queued follow-up"))
            .unwrap();
        assert!(first < second);
        assert!(
            messages[first]
                .to_string()
                .contains("data:image/png;base64,aW1hZ2U=")
        );
        assert_eq!(
            messages
                .iter()
                .filter(|m| m.to_string().contains("first queued follow-up"))
                .count(),
            1
        );
        assert!(
            messages
                .iter()
                .any(|m| m["role"] == "tool" && m["tool_call_id"] == "call-0")
        );
    }
}

#[tokio::test]
async fn stopped_run_keeps_undelivered_status_and_rejects_early_delivery() {
    let read = Arc::new(GatedRead {
        started: Notify::new(),
        release: Notify::new(),
        child_started: Notify::new(),
    });
    let harness = harness_with_read(vec![read_response(0)], 1_000_000, read.clone());
    let run =
        harness
            .runtime
            .delegate_background(Role::Worker, "task".into(), RunConfig::default());
    tokio::time::timeout(Duration::from_secs(20), read.started.notified())
        .await
        .unwrap();
    harness
        .runtime
        .send_message(run, "undelivered".into())
        .unwrap();
    harness.runtime.deliver_follow_ups_next_turn(run).unwrap();
    harness
        .runtime
        .stop(run, runtime::StopScope::SelfOnly)
        .unwrap();
    assert_eq!(
        harness.runtime.wait(run).await.unwrap(),
        AgentRunPhase::Stopped
    );
    assert_eq!(
        harness.runtime.follow_up_status(run).unwrap(),
        runtime::FollowUpStatus {
            pending: 1,
            next_turn_requested: true,
            closed: true,
        }
    );
    assert!(matches!(
        harness.runtime.deliver_follow_ups_next_turn(run),
        Err(runtime::RuntimeError::RunTerminated { .. })
    ));
}
