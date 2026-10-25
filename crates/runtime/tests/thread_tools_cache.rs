//! ADR 0041 cache contract: the read-only thread tools never perturb the
//! provider cache prefix. They are always offered right after `history_read`
//! with byte-identical specs, whether or not the run has a store or a thread,
//! and their results only append to the conversation.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

use event_bus::{AgentRunPhase, EventBus};
use providers::{
    ChatResponse, ContentBlock, FinishReason, Message, Role as MessageRole, ToolSpec, Usage,
};
use runtime::{
    AgentInvocationContext, AgentModel, AgentRuntime, Role, RunConfig, RunStore, RuntimeError,
};
use serde_json::{Value, json};
use storage::threads::ThreadUpsert;

const THREAD_TOOLS: [&str; 3] = ["thread_search", "thread_grep", "thread_read"];

/// Records every request's messages and offered tools; replies from a script.
#[derive(Default)]
struct Recording {
    script: Mutex<VecDeque<ChatResponse>>,
    requests: Mutex<Vec<(Vec<Message>, Vec<ToolSpec>)>>,
}

impl Recording {
    fn new(script: Vec<ChatResponse>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script.into()),
            requests: Mutex::default(),
        })
    }

    fn requests(&self) -> Vec<(Vec<Message>, Vec<ToolSpec>)> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl AgentModel for Recording {
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.requests
            .lock()
            .unwrap()
            .push((messages.to_vec(), tools.to_vec()));
        Ok(self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| response(vec![text("done")], FinishReason::Stop)))
    }

    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "thread-tools-cache".into()
    }
}

fn response(content: Vec<ContentBlock>, finish_reason: FinishReason) -> ChatResponse {
    ChatResponse {
        message: Message {
            role: MessageRole::Assistant,
            content,
        },
        finish_reason,
        usage: Usage::default(),
    }
}

fn text(text: &str) -> ContentBlock {
    ContentBlock::Text { text: text.into() }
}

fn call(id: &str, name: &str, input: Value) -> ChatResponse {
    response(
        vec![ContentBlock::ToolUse {
            id: id.into(),
            name: name.into(),
            input,
        }],
        FinishReason::ToolUse,
    )
}

/// A runtime with a durable store holding one saved thread of `project`.
struct Stored {
    runtime: AgentRuntime,
    _storage: storage::Storage,
    _db: tempfile::TempDir,
}

fn stored(model: Arc<dyn AgentModel>, project: &Path) -> Stored {
    let db = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: db.path().join("threads.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let bus = Arc::new(EventBus::new(1024));
    let runtime = AgentRuntime::new(bus.clone(), Arc::new(tools::ToolExecutor::new(bus)), model)
        .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    let handle = storage.handle();
    handle
        .upsert_thread_runtime(&ThreadUpsert {
            thread_id: "sibling".into(),
            project_root: Some(
                project
                    .canonicalize()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            ),
            root_run_id: Some("run-50".into()),
            title: Some("cache design".into()),
            created_at_ns: 1,
            updated_at_ns: 1,
            ..Default::default()
        })
        .unwrap();
    let messages = vec![
        Message {
            role: MessageRole::User,
            content: vec![text("design the cache layer")],
        },
        Message {
            role: MessageRole::Assistant,
            content: vec![text("the cache keeps a stable prefix")],
        },
    ];
    handle
        .upsert_run_context(&storage::RunContextRecord {
            run_id: "run-50".into(),
            role: "Worker".into(),
            name: "chat:Worker:sibling".into(),
            parent_run_id: None,
            config_json: json!({"thread_id": "sibling"}).to_string(),
            messages_json: serde_json::to_string(&messages).unwrap(),
            checkpoints_json: "[]".into(),
            terminal_phase: "Done".into(),
            restorable: false,
            updated_at_ns: 2,
        })
        .unwrap();
    let record = storage::Database::open(&config)
        .unwrap()
        .run_context("run-50")
        .unwrap()
        .unwrap();
    use storage::thread_index::{
        BlockOrigin, BlockProvenance, MessageProvenance, ProvenanceEvidence, ProvenanceMap,
        ProvenanceStatus, SnapshotWrite,
    };
    handle
        .save_snapshot_bundle(&SnapshotWrite {
            record,
            expected_snapshot_revision: None,
            binding: None,
            status: ProvenanceStatus::Verified {
                evidence: ProvenanceEvidence::CompletePositionMap,
            },
            map: ProvenanceMap {
                version: 1,
                raw_message_count: 2,
                checkpoint_count: 0,
                messages: (0..2)
                    .map(|ordinal| MessageProvenance {
                        raw_index: ordinal,
                        ordinal,
                        blocks: vec![BlockProvenance {
                            block_index: 0,
                            origin: BlockOrigin::Native,
                            excluded_reason: None,
                        }],
                    })
                    .collect(),
            },
        })
        .unwrap();
    let database = storage::Database::open(&config).unwrap();
    let bundle = database.snapshot_bundle("run-50").unwrap().unwrap();
    let input = storage::thread_index::extract_snapshot_bundle(
        &bundle,
        &secret_guard::SecretRedactor::from_env(),
    )
    .unwrap();
    let conn = rusqlite::Connection::open(&config.db_path).unwrap();
    for (ordinal, block) in input.blocks.iter().enumerate() {
        conn.execute(
            "INSERT INTO conversation_blocks(run_id, source_kind, source_key, ordinal, \
             block_index, role, kind, truncated, hash, text) \
             VALUES ('run-50', 'message', ?1, ?2, 0, ?3, 'text', 0, ?4, ?5)",
            rusqlite::params![
                format!("{ordinal}:0"),
                ordinal,
                if ordinal == 0 { "user" } else { "assistant" },
                block.hash,
                block.text
            ],
        )
        .unwrap();
    }
    conn.execute(
        "UPDATE thread_index_runs SET state = 'indexed', \
        indexed_revision = source_revision WHERE run_id = 'run-50'",
        [],
    )
    .unwrap();
    Stored {
        runtime,
        _storage: storage,
        _db: db,
    }
}

fn names(tools: &[ToolSpec]) -> Vec<&str> {
    tools.iter().map(|spec| spec.name.as_str()).collect()
}

/// The thread tools immediately follow `history_read`, in their fixed order.
fn assert_fixed_position(tools: &[ToolSpec]) {
    let names = names(tools);
    let history = names
        .iter()
        .position(|name| *name == "history_read")
        .unwrap_or_else(|| panic!("history_read is not offered: {names:?}"));
    assert_eq!(names[history + 1..history + 4], THREAD_TOOLS, "{names:?}");
}

fn thread_specs(tools: &[ToolSpec]) -> String {
    let specs: Vec<&ToolSpec> = tools
        .iter()
        .filter(|spec| THREAD_TOOLS.contains(&spec.name.as_str()))
        .collect();
    assert_eq!(specs.len(), THREAD_TOOLS.len());
    let actual = serde_json::to_string(&specs).unwrap();
    let expected: Vec<ToolSpec> =
        serde_json::from_str(include_str!("fixtures/thread_tools_phase_b.json")).unwrap();
    assert_eq!(actual, serde_json::to_string(&expected).unwrap());
    actual
}

#[tokio::test]
async fn thread_tool_calls_keep_tool_specs_fixed_and_the_wire_prefix_append_only() {
    // Given: a chat run whose project holds another saved thread.
    let project = tempfile::tempdir().unwrap();
    let model = Recording::new(vec![
        call(
            "c1",
            "thread_search",
            json!({
                "query": ["cache", "prefix"], "kinds": ["conversation", "tool_result"]
            }),
        ),
        call(
            "c2",
            "thread_grep",
            json!({"thread": "sibling", "pattern": "cache"}),
        ),
        call("c3", "thread_read", json!({"thread": "sibling"})),
        call(
            "c4",
            "thread_read",
            json!({"thread": "sibling", "view": "messages"}),
        ),
        call("c5", "thread_read", json!({"thread": "missing"})),
    ]);
    let fixture = stored(model.clone(), project.path());

    // When: the real agent loop dispatches every thread tool over successive turns.
    let run = fixture
        .runtime
        .delegate_chat(
            "caller",
            Role::Worker,
            "compare with the sibling thread".into(),
            RunConfig {
                project_root: Some(project.path().into()),
                ..RunConfig::default()
            },
        )
        .unwrap();
    assert_eq!(fixture.runtime.wait(run).await, Ok(AgentRunPhase::Done));

    // Then: the offered tools never change and history only grows at its end.
    let requests = model.requests();
    assert_eq!(requests.len(), 6);
    let first_tools = serde_json::to_string(&requests[0].1).unwrap();
    for (turn, (_, tools)) in requests.iter().enumerate() {
        assert_fixed_position(tools);
        assert_eq!(
            serde_json::to_string(tools).unwrap(),
            first_tools,
            "request {turn} changed the offered tools"
        );
    }
    for (turn, pair) in requests.windows(2).enumerate() {
        assert!(
            pair[1].0.starts_with(&pair[0].0),
            "model request {} rewrote a previously sent message",
            turn + 1
        );
    }
    let results: Vec<(String, bool)> = requests
        .last()
        .unwrap()
        .0
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                content, is_error, ..
            } => Some((
                content
                    .iter()
                    .map(|part| part.text_or_image_note().into_owned())
                    .collect(),
                *is_error,
            )),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 5, "{results:?}");
    for (output, is_error) in &results[..4] {
        assert!(!is_error, "{output}");
        assert!(output.contains("untrusted input"), "{output}");
        assert!(output.len() <= 16 * 1024);
    }
    assert!(results[1].0.contains("stable prefix"), "{}", results[1].0);
    assert!(results[0].0.contains("source_revision"), "{}", results[0].0);
    assert!(results[4].1);
    assert!(
        results[4]
            .0
            .starts_with("thread_denied: missing is not visible"),
        "{}",
        results[4].0
    );
}

#[tokio::test]
async fn thread_tool_specs_do_not_depend_on_a_store_a_thread_or_the_role() {
    // Given: runs without a store (worker and orchestrator) and a chat run with one.
    let project = tempfile::tempdir().unwrap();
    let mut offered = Vec::new();
    for role in [Role::Worker, Role::Orchestrator] {
        let model = Recording::new(Vec::new());
        let bus = Arc::new(EventBus::new(256));
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(tools::ToolExecutor::new(bus)),
            model.clone(),
        );
        let run = runtime.delegate_background(role, "no store".into(), RunConfig::default());
        assert_eq!(runtime.wait(run).await, Ok(AgentRunPhase::Done));
        offered.push(model.requests().remove(0).1);
    }
    let model = Recording::new(Vec::new());
    let fixture = stored(model.clone(), project.path());
    let run = fixture
        .runtime
        .delegate_chat(
            "caller",
            Role::Worker,
            "with a store".into(),
            RunConfig {
                project_root: Some(project.path().into()),
                ..RunConfig::default()
            },
        )
        .unwrap();
    assert_eq!(fixture.runtime.wait(run).await, Ok(AgentRunPhase::Done));
    offered.push(model.requests().remove(0).1);

    // Then: the thread tools sit at the same place with byte-identical specs.
    let expected = thread_specs(&offered[0]);
    for tools in &offered {
        assert_fixed_position(tools);
        assert_eq!(thread_specs(tools), expected);
    }
}

#[tokio::test]
async fn indexed_search_preserves_http_wire_prefix_and_input_derived_cache() {
    use mock_openai::cache_contract::{CacheProtocol, assert_append_only};
    use mock_openai::{ScriptedResponse, StreamingMockOpenAi};
    const MODEL: &str = "mock-model";
    let mock = StreamingMockOpenAi::spawn_with_prompt_cache(vec![
        ScriptedResponse::tool_call(
            "s1",
            MODEL,
            0,
            "search",
            "thread_search",
            [json!({"query":"stable prefix", "kinds":["conversation"]}).to_string()],
        ),
        ScriptedResponse::tool_call(
            "s2",
            MODEL,
            0,
            "grep",
            "thread_grep",
            [json!({"thread":"sibling", "pattern":"cache"}).to_string()],
        ),
        ScriptedResponse::text_stream("s3", MODEL, ["done"]),
    ]);
    let config = config::Config {
        providers: std::collections::BTreeMap::from([(
            "local".into(),
            config::ProviderProfileConfig {
                provider_type: config::ProviderTypeConfig::OpenAiCompatible,
                api_protocol: config::ApiProtocolConfig::OpenAiCompletions,
                base_url: mock.base_url(),
                credential: config::CredentialRefConfig::Env {
                    var: "EVORCH_THREAD_CACHE_TEST_KEY".into(),
                },
                models: vec![config::ModelEntryConfig::enabled(MODEL)],
                default_model: MODEL.into(),
                excluded_models: vec![],
                edit_tool: None,
            },
        )]),
        routing: config::RoutingConfig {
            routes: std::collections::BTreeMap::from([(
                "worker".into(),
                vec![config::RouteCandidateConfig {
                    profile: "local".into(),
                    model: Some(MODEL.into()),
                    auto: false,
                }],
            )]),
            ..Default::default()
        },
        compaction: config::CompactionConfig {
            context_window_tokens: 1_000_000,
            ..Default::default()
        },
        ..Default::default()
    };
    let project = tempfile::tempdir().unwrap();
    let bus = Arc::new(EventBus::new(1024));
    let mut events = bus.subscribe();
    let credentials =
        sandbox::credential::FileCredentialStore::open(project.path().join("credentials")).unwrap();
    let model = runtime::compose::compose_routed_model(
        &config,
        routing::ComposeDeps {
            credential_store: Arc::new(credentials),
            event_bus: Some(bus),
            env: Arc::new(routing::MapEnv::from_iter([(
                "EVORCH_THREAD_CACHE_TEST_KEY",
                "offline-key",
            )])),
            catalog: model::ModelCatalog::new(),
            factory: Default::default(),
        },
        Some(test_support::model_catalog_dir()),
    )
    .unwrap();
    let fixture = stored(model, project.path());
    let run = fixture
        .runtime
        .delegate_chat(
            "caller",
            Role::Worker,
            "Find a stable prefix in previous conversations.".into(),
            RunConfig {
                project_root: Some(project.path().into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(fixture.runtime.wait(run).await, Ok(AgentRunPhase::Done));
    let requests: Vec<_> = mock
        .recorded_requests()
        .into_iter()
        .filter(|r| r.path == "/v1/chat/completions")
        .map(|r| r.body)
        .collect();
    assert_eq!(requests.len(), 3);
    for pair in requests.windows(2) {
        assert_append_only(CacheProtocol::OpenAi, &pair[0], &pair[1]).unwrap();
    }
    let search_output = requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["tool_call_id"] == "search")
        .unwrap()["content"]
        .as_str()
        .unwrap();
    assert!(search_output.contains("source_revision"), "{search_output}");
    assert!(search_output.contains("stable prefix"), "{search_output}");
    let mut usage = Vec::new();
    while usage.len() < 3 {
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap();
        if let event_bus::EventKind::Provider(event_bus::ProviderEvent::RequestCompleted {
            input_tokens,
            cache_read_tokens,
            ..
        }) = event.kind
        {
            usage.push((input_tokens, cache_read_tokens));
        }
    }
    assert_eq!(usage.len(), 3);
    for pair in usage.windows(2) {
        assert!(pair[0].0 > 0);
        assert_eq!(
            pair[1].1, pair[0].0,
            "the input-derived mock must reuse the full previous prefix"
        );
    }
}
