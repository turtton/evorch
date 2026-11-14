//! Local peer plus production runtime, routing, HTTP/SSE and input-derived cache.
mod claude;
use async_trait::async_trait;
use config::{
    ApiProtocolConfig, Config, CredentialRefConfig, ModelPresetConfig, ProviderProfileConfig,
    ProviderTypeConfig, RouteCandidateConfig, SummarizerKind,
};
use event_bus::{
    AgentRunPhase, Event, EventBus, EventKind, EventReceiver, LifecycleEvent, ProviderEvent,
};
use mock_openai::cache_contract::{CacheProtocol, assert_append_only};
use mock_openai::{ScriptedResponse, StreamingMockOpenAi};
use routing::MapEnv;
use runtime::{
    AgentRuntime, ModelSource, Role, RunConfig, RunId, RunStore, RuntimeComposition,
    compose_runtime, mcp::ProjectMcpService,
};
use sandbox::credential::{CredentialStore, FileCredentialStore, Secret};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use tools::{
    NetworkGuard, ToolExecutor,
    mcp::{
        McpError, McpErrorKind,
        approval::{ApprovalLedger, inspect_stdio},
        names::McpNameBindings,
        secrets::credential_key,
        transport::*,
        types::*,
    },
};
pub const MODEL: &str = "mock-model";
pub const CLAUDE_MODEL: &str = "claude-sonnet-4-6";
pub const READ: &str = "mcp__fixture__read";
pub const GOAL: &str = "Inspect remote evidence without changing the sent prefix";
#[derive(Clone)]
pub enum Step {
    Call(String),
    Text(String),
}
pub fn call(name: &str) -> Step {
    Step::Call(name.into())
}
pub fn answer() -> Step {
    Step::Text("Retained analysis with evidence. ".repeat(150))
}
pub fn definitions() -> Vec<Value> {
    ["zebra", "read", "odd.tool"].into_iter().map(|name| json!({"name":name,
        "description":format!("Remote {name} documentation"),
        "inputSchema":{"type":"object","properties":{"index":{"type":"integer"}},"required":["index"]}})).collect()
}
pub fn hashed_name() -> String {
    let key = RemoteToolKey {
        server_id: "fixture".into(),
        tool_name: "odd.tool".into(),
    };
    McpNameBindings::new([key.clone()])
        .unwrap()
        .public_name(&key)
        .unwrap()
        .into()
}
pub struct Peer {
    pub tools: Mutex<Vec<Value>>,
    pub calls: Mutex<Vec<String>>,
    pub connections: AtomicUsize,
    pub lists: AtomicUsize,
    pub changed: AtomicBool,
    pub notifications: AtomicUsize,
}
impl Default for Peer {
    fn default() -> Self {
        Self {
            tools: Mutex::new(definitions()),
            calls: Mutex::new(vec![]),
            connections: AtomicUsize::new(0),
            lists: AtomicUsize::new(0),
            changed: AtomicBool::new(false),
            notifications: AtomicUsize::new(0),
        }
    }
}
struct Session {
    peer: Arc<Peer>,
    slot: Mutex<Option<Box<dyn McpProcessPermit>>>,
}
#[derive(Clone)]
struct Factory(Arc<Peer>);
#[async_trait]
impl McpTransportFactory for Factory {
    async fn connect(
        &self,
        server: ResolvedMcpServer,
        permit: ApprovalPermit,
        limiter: Arc<dyn McpProcessLimiter>,
        _: CancelContext,
    ) -> Result<Arc<dyn McpTransport>, McpError> {
        assert!(!permit.is_revoked());
        assert_eq!(permit.fingerprint(), server.fingerprint);
        let slot = limiter
            .try_acquire(&server.scope.canonical_root)
            .map_err(error)?;
        self.0.connections.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(Session {
            peer: self.0.clone(),
            slot: Mutex::new(Some(slot)),
        }))
    }
}
#[async_trait]
impl McpTransport for Session {
    async fn exchange(
        &self,
        request: RpcRequest,
        _: CancelContext,
    ) -> Result<McpResponseEnvelope, McpError> {
        let mut frames = Vec::new();
        let result = match request.method.as_str() {
            "server/discover" => {
                json!({"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{}},"ttlMs":0,"cacheScope":"private"})
            }
            "tools/list" => {
                self.peer.lists.fetch_add(1, Ordering::SeqCst);
                json!({"resultType":"complete","ttlMs":0,"cacheScope":"private","tools":self.peer.tools.lock().unwrap().clone()})
            }
            "tools/call" => {
                let name = request.params["name"].as_str().unwrap().to_owned();
                self.peer.calls.lock().unwrap().push(name.clone());
                if self.peer.changed.swap(false, Ordering::SeqCst) {
                    frames.push(RpcFrame::Notification(RpcNotification {
                        jsonrpc: "2.0".into(),
                        method: "notifications/tools/list_changed".into(),
                        params: None,
                    }));
                    self.peer.notifications.fetch_add(1, Ordering::SeqCst);
                }
                json!({"resultType":"complete","content":[{"type":"text","text":format!("remote-ok-{name}: evidence\n").repeat(80)}]})
            }
            other => panic!("unexpected MCP method {other}"),
        };
        frames.push(RpcFrame::Result {
            jsonrpc: "2.0".into(),
            id: request.id,
            result,
        });
        McpResponseEnvelope::new(Some(200), McpResponseHeaders::default(), frames, 512)
            .map_err(error)
    }
    async fn notify(&self, _: RpcNotification) -> Result<(), McpError> {
        Ok(())
    }
    async fn respond(&self, _: RpcFrame) -> Result<(), McpError> {
        panic!("no server request")
    }
    async fn cancel(&self, _: CancelRequest) -> Result<(), McpError> {
        Ok(())
    }
    async fn shutdown(&self, _: ShutdownReason) -> Result<(), McpError> {
        self.slot.lock().unwrap().take();
        Ok(())
    }
}
fn error(kind: McpErrorKind) -> McpError {
    McpError {
        server: "fixture".into(),
        method: "tools/call",
        request_id: None,
        kind,
    }
}
pub struct Project {
    pub directory: test_socket_dir::SplitDir,
    pub approvals: Arc<ApprovalLedger>,
    pub secrets: Arc<FileCredentialStore>,
    pub peer: Arc<Peer>,
    pub service: Arc<ProjectMcpService>,
    pub storage: storage::Storage,
    pub storage_config: storage::StorageConfig,
}
impl Project {
    pub fn new() -> Self {
        let directory = test_socket_dir::SplitDir::new();
        std::fs::create_dir(directory.path().join(".evorch")).unwrap();
        let approvals =
            Arc::new(ApprovalLedger::at_user_config_dir(directory.path().join("user")).unwrap());
        let secrets =
            Arc::new(FileCredentialStore::open(directory.path().join("secrets")).unwrap());
        let peer = Arc::new(Peer::default());
        let factory = Arc::new(Factory(peer.clone()));
        let service = Arc::new(
            ProjectMcpService::new(
                McpCredentialBackend::Available(secrets.clone()),
                approvals.clone(),
                Arc::new(NetworkGuard::new().unwrap()),
            )
            .with_transport_factories(factory.clone(), factory),
        );
        let storage_config = storage::StorageConfig {
            db_path: directory.path().join("runs.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(storage_config.clone()).unwrap();
        let result = Self {
            directory,
            approvals,
            secrets,
            peer,
            service,
            storage,
            storage_config,
        };
        result.write_config("");
        result.rotate_secret("secret-generation-one");
        result.approve();
        result
    }
    pub fn root(&self) -> &Path {
        self.directory.path()
    }

    /// Short directory for the ownership root; the sockets must fit in `sun_path`.
    #[allow(dead_code)]
    pub fn sockets(&self) -> &Path {
        self.directory.sockets()
    }
    pub fn scope(&self) -> ApprovalScope {
        ApprovalScope {
            canonical_root: self.root().canonicalize().unwrap(),
            server_id: "fixture".into(),
        }
    }
    pub fn write_config(&self, extra: &str) {
        let command = std::env::current_exe().unwrap().canonicalize().unwrap();
        std::fs::write(self.root().join(".evorch/mcp.toml"),format!("version=1\n[servers.fixture]\ntype=\"stdio\"\ncommand={command:?}\n{extra}\n[servers.fixture.env]\nTOKEN={{keychain=\"token\"}}\n")).unwrap();
    }
    pub fn rotate_secret(&self, value: &str) {
        self.secrets
            .set(
                &credential_key(&self.scope(), "token"),
                &Secret::from(value.to_owned()),
            )
            .unwrap();
    }
    pub fn approve(&self) {
        let config = config::mcp::load_project(self.root()).unwrap();
        let definition = config.servers["fixture"].definition.as_ref().unwrap();
        let config::mcp::McpTransportDefinition::Stdio { command, .. } = &definition.transport
        else {
            panic!("stdio")
        };
        let command = config::mcp::expand::expand_template(command, &|_| None).unwrap();
        let inspection = inspect_stdio(
            self.scope(),
            config.revision,
            definition,
            &Path::new(&command).canonicalize().unwrap(),
            &self.scope().canonical_root,
        )
        .unwrap();
        let preview = self.approvals.preview(&inspection).unwrap();
        self.approvals
            .approve(
                &ConfirmMcpApproval {
                    preview_id: preview.id,
                    scope: preview.scope,
                    expected_revision: preview.source_revision,
                    expected_fingerprint: preview.fingerprint,
                },
                &inspection,
            )
            .unwrap();
    }
}
pub enum Provider {
    Api(StreamingMockOpenAi),
    Claude(claude::Claude),
}
impl Provider {
    pub async fn new(subscription: bool) -> Self {
        if subscription {
            Self::Claude(claude::Claude::new().await)
        } else {
            Self::Api(StreamingMockOpenAi::spawn_with_prompt_cache(vec![]))
        }
    }
    pub fn model(&self) -> &str {
        match self {
            Self::Api(_) => MODEL,
            Self::Claude(_) => CLAUDE_MODEL,
        }
    }
    pub fn protocol(&self) -> CacheProtocol {
        match self {
            Self::Api(_) => CacheProtocol::OpenAi,
            Self::Claude(_) => CacheProtocol::Anthropic,
        }
    }
    pub fn enqueue(&self, steps: Vec<Step>) {
        match self {
            Self::Api(mock) => {
                let offset = self.requests().len();
                mock.enqueue(steps.into_iter().enumerate().map(|(i, step)| {
                    let i = i + offset;
                    match step {
                        Step::Call(name) => ScriptedResponse::tool_call(
                            &format!("response-{i}"),
                            MODEL,
                            0,
                            &format!("call-{i}"),
                            &name,
                            [json!({"index":i}).to_string()],
                        ),
                        Step::Text(text) => {
                            ScriptedResponse::text_stream(&format!("response-{i}"), MODEL, [text])
                        }
                    }
                }))
            }
            Self::Claude(mock) => mock.enqueue(steps),
        }
    }
    pub fn requests(&self) -> Vec<Value> {
        match self {
            Self::Api(mock) => mock
                .recorded_requests()
                .into_iter()
                .filter(|r| r.path == "/v1/chat/completions")
                .map(|r| r.body)
                .collect(),
            Self::Claude(mock) => mock.requests(),
        }
    }
    pub fn assert_drained(&self) {
        match self {
            Self::Api(mock) => assert_eq!(mock.remaining_scripts(), 0),
            Self::Claude(mock) => mock.assert_drained(),
        }
    }
    pub fn tool_name(&self, public: &str) -> String {
        match self {
            Self::Api(_) => public.into(),
            Self::Claude(_) => format!("_mcp__evorch__{public}"),
        }
    }
    pub fn profile(&self) -> ProviderProfileConfig {
        let (provider_type, api_protocol, base_url, credential) = match self {
            Self::Api(mock) => (
                ProviderTypeConfig::OpenAiCompatible,
                ApiProtocolConfig::OpenAiCompletions,
                mock.base_url(),
                CredentialRefConfig::Env {
                    var: "MCP_CACHE_KEY".into(),
                },
            ),
            Self::Claude(mock) => (
                ProviderTypeConfig::AnthropicSubscription,
                ApiProtocolConfig::AnthropicMessages,
                mock.uri(),
                CredentialRefConfig::Keyring {
                    service: "evorch".into(),
                    account: "claude".into(),
                },
            ),
        };
        ProviderProfileConfig {
            provider_type,
            api_protocol,
            base_url,
            credential,
            models: vec![config::ModelEntryConfig::enabled(self.model())],
            default_model: self.model().into(),
            excluded_models: vec![],
            edit_tool: None,
        }
    }
}
pub fn configuration(provider: &Provider) -> Config {
    let mut config = Config::default();
    let mut profile = provider.profile();
    profile.models[0].preset = Some("output".into());
    config.providers.insert("fixture".into(), profile);
    config.model_presets.insert(
        "output".into(),
        ModelPresetConfig {
            max_output_tokens: Some(16_384),
            ..Default::default()
        },
    );
    config.routing.routes.insert(
        "worker".into(),
        vec![RouteCandidateConfig {
            profile: "fixture".into(),
            ..Default::default()
        }],
    );
    config.compaction.context_window_tokens = 1_000_000;
    config.compaction.keep_recent_tokens = 1;
    config.compaction.max_summary_bytes = 256;
    config.compaction.summarizer = SummarizerKind::Structural;
    config
}
pub struct Harness {
    pub runtime: AgentRuntime,
    pub receiver: EventReceiver,
}
pub fn harness(project: &Project, config: &Config) -> Harness {
    project.secrets.set("claude",&Secret::from(json!({"access_token":"fixture-mcp-token","refresh_token":"fixture-refresh","expires_at":4102444800u64}).to_string())).unwrap();
    let bus = Arc::new(EventBus::new(4096));
    let receiver = bus.subscribe();
    let runtime = compose_runtime(RuntimeComposition {
        config,
        bus: bus.clone(),
        executor: Arc::new(ToolExecutor::new(bus)),
        credential_store: project.secrets.clone(),
        env: Arc::new(MapEnv::from_iter([("MCP_CACHE_KEY", "fixture-key")])),
        model_source: ModelSource::Configured,
        workspace: None,
        model_catalog_dir: Some(test_support::model_catalog_dir()),
        user_config_dir: Some(project.root().join("empty-user")),
    })
    .unwrap()
    .runtime
    .with_mcp_service(project.service.clone())
    .with_run_store(RunStore::open(&project.storage_config, project.storage.handle()).unwrap())
    .with_compaction(config.compaction.clone());
    runtime
        .set_project_root(project.root().canonicalize().unwrap())
        .unwrap();
    Harness { runtime, receiver }
}
pub fn authority() -> RunConfig {
    RunConfig {
        interactive: true,
        keep_alive: true,
        ..Default::default()
    }
}
pub fn start(harness: &Harness) -> RunId {
    harness
        .runtime
        .delegate_background(Role::Worker, GOAL.into(), authority())
}
pub async fn through(
    receiver: &mut EventReceiver,
    run: RunId,
    expected: AgentRunPhase,
) -> Vec<Event> {
    let mut events = vec![];
    loop {
        let event = receiver.recv().await.unwrap();
        let reached = match &event.kind {
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to, .. })
                if run_id == &run.to_string() =>
            {
                if matches!(
                    to,
                    AgentRunPhase::Done | AgentRunPhase::Error | AgentRunPhase::Stopped
                ) {
                    assert_eq!(*to, expected, "unexpected terminal event {event:?}");
                }
                *to == expected
            }
            _ => false,
        };
        events.push(event);
        if reached {
            return events;
        }
    }
}
pub async fn stop(harness: &mut Harness, run: RunId) {
    harness
        .runtime
        .stop(run, runtime::StopScope::SelfOnly)
        .unwrap();
    assert_eq!(
        harness.runtime.wait(run).await.unwrap(),
        AgentRunPhase::Stopped
    );
}
pub fn assert_frozen(provider: &Provider, requests: &[Value]) {
    assert!(requests.len() >= 2);
    for pair in requests.windows(2) {
        assert!(
            pair[0]["tools"] == pair[1]["tools"],
            "schema, description and array order must remain frozen"
        );
        assert_append_only(provider.protocol(), &pair[0], &pair[1]).unwrap();
    }
}
pub fn assert_usage_reuse(events: &[Event], expected: usize) {
    let usage = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::Provider(ProviderEvent::RequestCompleted {
                input_tokens,
                cache_read_tokens,
                ..
            }) => Some((*input_tokens, *cache_read_tokens)),
            EventKind::Diagnostic(d) => {
                assert_ne!(d.code, "CacheRegression", "{d:?}");
                None
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(usage.len(), expected);
    assert_eq!(usage[0].1, 0);
    for pair in usage.windows(2) {
        assert!(
            pair[1].1 >= pair[0].0,
            "entire prior input must be cached: {pair:?}"
        );
    }
}
pub fn external_tools(provider: &Provider, request: &Value) -> Vec<Value> {
    request["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|tool| {
            let name = match provider {
                Provider::Api(_) => &tool["function"]["name"],
                Provider::Claude(_) => &tool["name"],
            };
            name.as_str().is_some_and(|n| {
                n.starts_with(&provider.tool_name("mcp__"))
                    || n.starts_with(&provider.tool_name("mcp_h__"))
            })
        })
        .cloned()
        .collect()
}
