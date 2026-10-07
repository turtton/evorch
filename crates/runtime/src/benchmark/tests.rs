use super::*;
use crate::{AgentInvocationContext, AgentModel, AgentRuntime, RunConfig};
use event_bus::EventBus;
use providers::{ChatResponse, ContentBlock, FinishReason, Usage};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone)]
struct Observed {
    role: Role,
    messages: Vec<Message>,
    tools: Vec<ToolSpec>,
    settings: Option<BenchmarkModelSettings>,
}

struct Flow {
    requests: Arc<Mutex<Vec<Observed>>>,
    root_calls: Arc<AtomicUsize>,
    worker_calls: AtomicUsize,
    settings: Option<BenchmarkModelSettings>,
    side_channel: bool,
}

fn settings(model: &str) -> BenchmarkModelSettings {
    BenchmarkModelSettings {
        protocol: model::ApiProtocol::OpenAiCompletions,
        preference: ModelPreference {
            profile: "mock".into(),
            model: Some(model.into()),
            reasoning_effort: Some("high".into()),
        },
        generation: config::GenerationOverridesConfig {
            temperature: Some(0.25),
            max_tokens: Some(321),
            ..Default::default()
        },
        service_tier: Some(providers::ServiceTier::Priority),
    }
}

fn response(content: ContentBlock, finish_reason: FinishReason) -> ChatResponse {
    ChatResponse {
        message: Message {
            role: providers::Role::Assistant,
            content: vec![content],
        },
        usage: Usage::default(),
        finish_reason,
    }
}

fn call(name: &str, input: serde_json::Value) -> ChatResponse {
    response(
        ContentBlock::ToolUse {
            id: name.into(),
            name: name.into(),
            input,
        },
        FinishReason::ToolUse,
    )
}

#[async_trait]
impl AgentModel for Flow {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "mock/model-a".into()
    }
    fn benchmark_settings(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[ToolSpec],
    ) -> Result<BenchmarkModelSettings, RuntimeError> {
        Ok(settings("model-a"))
    }
    fn freeze_for_benchmark(
        self: Arc<Self>,
        settings: BenchmarkModelSettings,
    ) -> Result<Arc<dyn AgentModel>, RuntimeError> {
        Ok(Arc::new(Self {
            requests: self.requests.clone(),
            root_calls: self.root_calls.clone(),
            worker_calls: AtomicUsize::new(0),
            settings: Some(settings),
            side_channel: self.side_channel,
        }))
    }
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.requests.lock().unwrap().push(Observed {
            role,
            messages: messages.to_vec(),
            tools: tools.to_vec(),
            settings: self.settings.clone(),
        });
        if role == Role::Orchestrator {
            if self.root_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                return Ok(call(
                    "delegate",
                    serde_json::json!({"target":{"role":"worker"},"prompt":"Update answer.txt"}),
                ));
            }
            return Ok(response(
                ContentBlock::Text {
                    text: "BASELINE_FUTURE_OUTCOME".into(),
                },
                FinishReason::Stop,
            ));
        }
        if self.worker_calls.fetch_add(1, Ordering::SeqCst) == 0 {
            if self.side_channel {
                return Ok(call(
                    "send",
                    serde_json::json!({"target":"run-1","message":"future?"}),
                ));
            }
            let settings = self.settings.as_ref().ok_or_else(|| RuntimeError::Model {
                reason: "benchmark child request must use frozen model settings".into(),
            })?;
            return Ok(call(
                "write",
                serde_json::json!({"path":"answer.txt","content":settings.preference.model}),
            ));
        }
        Ok(response(
            ContentBlock::Text {
                text: "local result".into(),
            },
            FinishReason::Stop,
        ))
    }
}

struct Recorder {
    checkpoint: Mutex<Option<BenchmarkCheckpoint>>,
    before: Mutex<Option<String>>,
    after: Mutex<Option<String>>,
    root_calls: Arc<AtomicUsize>,
}

#[async_trait]
impl BenchmarkRecorder for Recorder {
    async fn capture(&self, checkpoint: &BenchmarkCheckpoint) -> Result<(), String> {
        *self.before.lock().unwrap() =
            Some(std::fs::read_to_string(checkpoint.workspace_root.join("answer.txt")).unwrap());
        *self.checkpoint.lock().unwrap() =
            Some(serde_json::from_str(&serde_json::to_string(checkpoint).unwrap()).unwrap());
        Ok(())
    }
    async fn completed(
        &self,
        checkpoint: &BenchmarkCheckpoint,
        phase: AgentRunPhase,
        result: Option<&str>,
    ) -> Result<(), String> {
        assert_eq!(phase, AgentRunPhase::Done);
        assert_eq!(result, Some("local result"));
        assert_eq!(
            self.root_calls.load(Ordering::SeqCst),
            1,
            "parent must still be waiting"
        );
        *self.after.lock().unwrap() =
            Some(std::fs::read_to_string(checkpoint.workspace_root.join("answer.txt")).unwrap());
        Ok(())
    }
}

fn runtime(root: &std::path::Path, flow: Arc<dyn AgentModel>) -> AgentRuntime {
    let bus = Arc::new(EventBus::new(4096));
    let mut executor = tools::ToolExecutor::new(bus.clone());
    executor.register(Arc::new(tools::tools::Write)).unwrap();
    let sandbox = Arc::new(sandbox::DirectSandbox::new_unchecked());
    executor
        .register(Arc::new(tools::tools::Shell::new(sandbox.clone())))
        .unwrap();
    executor
        .register(Arc::new(tools::tools::GitDiff::new(sandbox)))
        .unwrap();
    executor.set_default_cwd(root.to_path_buf());
    AgentRuntime::new(bus, Arc::new(executor), flow).with_project_rules(Arc::new(
        crate::RulesSource::new(
            crate::ProjectTrust::Approved,
            crate::RulesSettings::from(&config::RulesConfig::default()),
            None,
            Some(root.to_path_buf()),
            None,
        ),
    ))
}

struct FailedCapture;

#[async_trait]
impl BenchmarkRecorder for FailedCapture {
    async fn capture(&self, _: &BenchmarkCheckpoint) -> Result<(), String> {
        Err("snapshot storage failed".into())
    }
}

#[tokio::test]
async fn failed_checkpoint_prevents_child_model_and_tools() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("answer.txt"), "before").unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = runtime(
        root.path(),
        flow(requests.clone(), Arc::new(AtomicUsize::new(0)), false),
    )
    .with_benchmark_recorder(
        BenchmarkSelector {
            role: Role::Worker,
            category: None,
            occurrence: 1,
        },
        Arc::new(FailedCapture),
    )
    .unwrap();
    let run = runtime.delegate_background(Role::Orchestrator, "task".into(), RunConfig::default());
    runtime.wait(run).await.unwrap();
    assert!(
        requests
            .lock()
            .unwrap()
            .iter()
            .all(|request| request.role == Role::Orchestrator)
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("answer.txt")).unwrap(),
        "before"
    );
}

struct ConcurrentDelegation(bool);

struct UnsupportedProcess(Arc<AtomicUsize>, &'static str);

#[async_trait]
impl AgentModel for UnsupportedProcess {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "mock/model-a".into()
    }
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(call(
            self.1,
            if self.1 == "shell" {
                serde_json::json!({"command":"echo background", "yield_ms":0})
            } else {
                serde_json::json!({})
            },
        ))
    }
}

#[tokio::test]
async fn recording_rejects_unsupported_process_and_goal_tools_before_execution() {
    let root = tempfile::tempdir().unwrap();
    for name in ["shell", "git_diff", "create_goal", "get_goal"] {
        let calls = Arc::new(AtomicUsize::new(0));
        let runtime = runtime(
            root.path(),
            Arc::new(UnsupportedProcess(calls.clone(), name)),
        )
        .with_benchmark_recorder(
            BenchmarkSelector {
                role: Role::Worker,
                category: None,
                occurrence: 1,
            },
            Arc::new(FailedCapture),
        )
        .unwrap();
        let run = runtime.delegate_background(Role::Worker, "task".into(), RunConfig::default());
        assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Error);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "unsupported tool terminates before another model turn"
        );
    }
}

#[tokio::test]
async fn recording_rejects_thread_bound_runs_and_initial_goals_before_model_request() {
    let root = tempfile::tempdir().unwrap();
    for initial_goal in [true, false] {
        let calls = Arc::new(AtomicUsize::new(0));
        let runtime = runtime(
            root.path(),
            Arc::new(UnsupportedProcess(calls.clone(), "create_goal")),
        )
        .with_benchmark_recorder(
            BenchmarkSelector {
                role: Role::Worker,
                category: None,
                occurrence: 1,
            },
            Arc::new(FailedCapture),
        )
        .unwrap();
        let run = if initial_goal {
            runtime.delegate_background(
                Role::Worker,
                "task".into(),
                RunConfig {
                    initial_thread_goal: Some(("objective".into(), vec!["criterion".into()])),
                    ..Default::default()
                },
            )
        } else {
            let run = runtime.reserve_run_id();
            runtime.bind_thread_root("benchmark-thread", run).unwrap();
            runtime
                .create_thread_goal(
                    "benchmark-thread",
                    run,
                    "objective".into(),
                    vec!["criterion".into()],
                )
                .unwrap();
            runtime.spawn_reserved(run, None, Role::Worker, "task", RunConfig::default())
        };
        assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Error);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}

#[async_trait]
impl AgentModel for ConcurrentDelegation {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "mock/model-a".into()
    }
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        let mut response = call(
            "delegate",
            serde_json::json!({"target":{"role":"worker"},"prompt":"work", "background":self.0}),
        );
        if !self.0 {
            response
                .message
                .content
                .extend(response.message.content.clone());
        }
        Ok(response)
    }
}

#[tokio::test]
async fn recording_rejects_background_and_parallel_delegation_before_children_start() {
    let root = tempfile::tempdir().unwrap();
    for background in [true, false] {
        let runtime = runtime(root.path(), Arc::new(ConcurrentDelegation(background)))
            .with_benchmark_recorder(
                BenchmarkSelector {
                    role: Role::Worker,
                    category: None,
                    occurrence: 1,
                },
                Arc::new(FailedCapture),
            )
            .unwrap();
        let run =
            runtime.delegate_background(Role::Orchestrator, "task".into(), RunConfig::default());
        assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Error);
        assert_eq!(
            runtime.list_agents().len(),
            1,
            "no concurrent child was registered"
        );
    }
}

fn flow(
    requests: Arc<Mutex<Vec<Observed>>>,
    root_calls: Arc<AtomicUsize>,
    side_channel: bool,
) -> Arc<Flow> {
    Arc::new(Flow {
        requests,
        root_calls,
        worker_calls: AtomicUsize::new(0),
        settings: None,
        side_channel,
    })
}

#[tokio::test]
async fn local_replay_preserves_input_and_settings_before_real_tool_writes() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("answer.txt"), "before").unwrap();
    std::fs::write(root.path().join("AGENTS.md"), "original rules").unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let root_calls = Arc::new(AtomicUsize::new(0));
    let recorder = Arc::new(Recorder {
        checkpoint: Mutex::new(None),
        before: Mutex::new(None),
        after: Mutex::new(None),
        root_calls: root_calls.clone(),
    });
    let baseline = runtime(
        root.path(),
        flow(requests.clone(), root_calls.clone(), false),
    )
    .with_benchmark_recorder(
        BenchmarkSelector {
            role: Role::Worker,
            category: None,
            occurrence: 1,
        },
        recorder.clone(),
    )
    .unwrap();
    let run = baseline.delegate_background(
        Role::Orchestrator,
        "whole task".into(),
        RunConfig::default(),
    );
    assert_eq!(baseline.wait(run).await.unwrap(), AgentRunPhase::Done);
    assert_eq!(
        baseline.run_result(run).unwrap().as_deref(),
        Some("BASELINE_FUTURE_OUTCOME")
    );
    assert_eq!(recorder.before.lock().unwrap().as_deref(), Some("before"));
    assert_eq!(recorder.after.lock().unwrap().as_deref(), Some("model-a"));
    let checkpoint = recorder.checkpoint.lock().unwrap().clone().unwrap();
    let original = requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request.role == Role::Worker)
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(original[0].messages, checkpoint.messages);
    for request in &original {
        assert_eq!(request.settings.as_ref(), Some(&checkpoint.model));
    }
    assert!(
        !serde_json::to_string(&checkpoint)
            .unwrap()
            .contains("BASELINE_FUTURE_OUTCOME")
    );
    assert!(checkpoint.tools.iter().all(|tool| {
        tool.name != "git_diff" && (!crate::is_meta_op(&tool.name) || tool.name == "finish")
    }));
    let shell = checkpoint
        .tools
        .iter()
        .find(|tool| tool.name == "shell")
        .unwrap();
    let properties = shell.input_schema["properties"].as_object().unwrap();
    assert!(properties.contains_key("command"));
    for hidden in [
        "yield_ms",
        "interactive",
        "job_id",
        "action",
        "sandbox_access",
    ] {
        assert!(!properties.contains_key(hidden));
    }
    let root_tools = requests.lock().unwrap()[0].tools.clone();
    assert!(!root_tools.iter().any(|tool| tool.name == "git_diff"));
    for hidden in [
        "create_goal",
        "get_goal",
        "submit_goal_check",
        "submit_goal_review",
    ] {
        assert!(!root_tools.iter().any(|tool| tool.name == hidden));
    }
    let delegate = root_tools
        .iter()
        .find(|tool| tool.name == "delegate")
        .unwrap();
    assert!(
        delegate.input_schema["properties"]
            .get("background")
            .is_none()
    );
    for model in ["model-a", "model-b"] {
        std::fs::write(root.path().join("answer.txt"), "before").unwrap();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let replay = runtime(
            root.path(),
            flow(observed.clone(), Arc::new(AtomicUsize::new(0)), false),
        );
        let replay_run = replay
            .replay_benchmark(
                checkpoint.clone(),
                root.path().to_path_buf(),
                settings(model).preference,
            )
            .unwrap();
        assert_eq!(replay.wait(replay_run).await.unwrap(), AgentRunPhase::Done);
        assert_eq!(
            replay.run_result(replay_run).unwrap().as_deref(),
            Some("local result")
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("answer.txt")).unwrap(),
            model
        );
        let observed = observed.lock().unwrap();
        assert_eq!(observed[0].messages, original[0].messages);
        assert_eq!(observed[0].tools, original[0].tools);
        for request in observed.iter() {
            assert_eq!(
                request.settings.as_ref().unwrap().generation,
                checkpoint.model.generation
            );
            assert_eq!(
                request.settings.as_ref().unwrap().service_tier,
                checkpoint.model.service_tier
            );
            assert!(request.messages.starts_with(&checkpoint.messages));
            assert!(
                !serde_json::to_string(&request.messages)
                    .unwrap()
                    .contains("BASELINE_FUTURE_OUTCOME")
            );
        }
        if model == "model-a" {
            assert_eq!(observed[1].messages, original[1].messages);
        }
    }
    let mut invalid = checkpoint.clone();
    invalid.tools.reverse();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let replay = runtime(
        root.path(),
        flow(observed.clone(), Arc::new(AtomicUsize::new(0)), false),
    );
    let run = replay
        .replay_benchmark(
            invalid,
            root.path().to_path_buf(),
            settings("model-a").preference,
        )
        .unwrap();
    assert_eq!(replay.wait(run).await.unwrap(), AgentRunPhase::Error);
    assert!(observed.lock().unwrap().is_empty());

    let replay = runtime(
        root.path(),
        flow(
            Arc::new(Mutex::new(Vec::new())),
            Arc::new(AtomicUsize::new(0)),
            true,
        ),
    );
    let run = replay
        .replay_benchmark(
            checkpoint,
            root.path().to_path_buf(),
            settings("model-b").preference,
        )
        .unwrap();
    assert_eq!(
        replay.wait(run).await.unwrap(),
        AgentRunPhase::Error,
        "parent side channels cannot consume future answers"
    );
}
