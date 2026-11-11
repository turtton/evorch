use super::*;
use crate::{AgentInvocationContext, AgentModel, AgentRuntime, Role, RunConfig, RuntimeError};
use event_bus::EventBus;
use providers::{ChatRequest, ChatResponse, ContentBlock, FinishReason, Message, ToolSpec};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::{Notify, Semaphore, mpsc};
use tools::{Permissions, Tool, ToolError, ToolExecutionMode, ToolExecutor, ToolResult};

pub(super) struct GateModel {
    pub(super) admission: bool,
    pub(super) admission_calls: AtomicUsize,
    pub(super) admission_entered: Notify,
    pub(super) admission_release: Semaphore,
    pub(super) calls: AtomicUsize,
    pub(super) count: usize,
    pub(super) entered: Notify,
    pub(super) release: Semaphore,
    pub(super) input: Mutex<Option<ProviderInput>>,
}
#[async_trait::async_trait]
impl AgentModel for GateModel {
    fn requires_admission(&self) -> bool {
        self.admission
    }
    async fn admit(&self, _: &AgentInvocationContext, _: Role) -> Result<(), RuntimeError> {
        self.admission_calls.fetch_add(1, Ordering::SeqCst);
        self.admission_entered.notify_one();
        self.admission_release.acquire().await.unwrap().forget();
        Ok(())
    }
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "test".into()
    }
    fn checkpoint_provider_input(&self, _: &str) -> Option<ProviderInput> {
        self.input.lock().unwrap().clone()
    }
    async fn complete(
        &self,
        invocation: &AgentInvocationContext,
        _: Role,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.input.lock().unwrap() = Some(ProviderInput {
            profile: "test".into(),
            protocol: model::ApiProtocol::OpenAiCompletions,
            edit_tool: config::EditToolConfig::Edit,
            affinity_key: invocation.run_id.clone(),
            replay: None,
            request: ChatRequest {
                model: "test".into(),
                messages: messages.to_vec(),
                tools: tools.to_vec(),
                tool_choice: None,
                temperature: None,
                max_tokens: None,
                reasoning_effort: None,
                service_tier: None,
                output_schema: None,
                observation: None,
                responses_replay: None,
            },
        });
        self.entered.notify_one();
        self.release.acquire().await.unwrap().forget();
        Ok(ChatResponse {
            message: Message {
                role: providers::Role::Assistant,
                content: (0..self.count)
                    .map(|index| ContentBlock::ToolUse {
                        id: format!("call-{index}"),
                        name: "read".into(),
                        input: json!({"index":index}),
                    })
                    .collect(),
            },
            usage: Default::default(),
            usage_reported: true,
            finish_reason: FinishReason::ToolUse,
        })
    }
}
pub(super) struct GateRead {
    pub(super) entered: mpsc::UnboundedSender<usize>,
    pub(super) completed: mpsc::UnboundedSender<usize>,
    pub(super) release: Vec<Arc<Semaphore>>,
}
#[async_trait::async_trait]
impl Tool for GateRead {
    fn name(&self) -> &str {
        "read"
    }
    fn schema(&self) -> Value {
        json!({"type":"object","required":["index"],"properties":{"index":{"type":"integer"}}})
    }
    fn permissions(&self) -> Permissions {
        Permissions::read_only()
    }
    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Shared
    }
    async fn execute(&self, input: Value) -> Result<ToolResult, ToolError> {
        let index = input["index"].as_u64().unwrap() as usize;
        self.entered.send(index).unwrap();
        self.release[index].acquire().await.unwrap().forget();
        self.completed.send(index).unwrap();
        Ok(ToolResult::success(format!("result-{index}")))
    }
}
pub(super) struct Fixture {
    pub(super) _dir: tempfile::TempDir,
    pub(super) config: storage::StorageConfig,
    pub(super) _storage: storage::Storage,
    pub(super) runtime: AgentRuntime,
    pub(super) model: Arc<GateModel>,
    pub(super) entered: mpsc::UnboundedReceiver<usize>,
    pub(super) completed: mpsc::UnboundedReceiver<usize>,
    pub(super) release: Vec<Arc<Semaphore>>,
}
impl Fixture {
    pub(super) fn new(count: usize) -> Self {
        Self::with_admission(count, false)
    }
    pub(super) fn with_admission(count: usize, admission: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: dir.path().join("runs.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let bus = Arc::new(EventBus::new(256));
        let model = Arc::new(GateModel {
            admission,
            admission_calls: AtomicUsize::new(0),
            admission_entered: Notify::new(),
            admission_release: Semaphore::new(0),
            calls: AtomicUsize::new(0),
            count,
            entered: Notify::new(),
            release: Semaphore::new(0),
            input: Mutex::default(),
        });
        let (entered_tx, entered) = mpsc::unbounded_channel();
        let (completed_tx, completed) = mpsc::unbounded_channel();
        let release: Vec<_> = (0..count).map(|_| Arc::new(Semaphore::new(0))).collect();
        let mut executor = ToolExecutor::new(bus.clone());
        executor
            .register(Arc::new(GateRead {
                entered: entered_tx,
                completed: completed_tx,
                release: release.clone(),
            }))
            .unwrap();
        let runtime = AgentRuntime::new(bus, Arc::new(executor), model.clone())
            .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
        Self {
            _dir: dir,
            config,
            _storage: storage,
            runtime,
            model,
            entered,
            completed,
            release,
        }
    }
    pub(super) fn start(&self) -> RunId {
        self.runtime.delegate_background(
            Role::Worker,
            "work".into(),
            RunConfig {
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )
    }
    pub(super) async fn cleanup(&self, run: RunId) {
        self.runtime.cancel(run).unwrap();
        self.runtime.wait(run).await.unwrap();
    }
    pub(super) fn intent(&self, run: RunId) -> ResumeIntent {
        self.runtime
            .resume_intents()
            .unwrap()
            .into_iter()
            .find(|intent| intent.run_id == run)
            .unwrap()
    }
}
