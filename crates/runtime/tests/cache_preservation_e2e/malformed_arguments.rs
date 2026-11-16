//! Full runtime -> Anthropic SSE -> error result -> stable cached continuation.
use super::*;
use std::sync::Mutex;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

#[derive(Default)]
struct NativeCache {
    state: Mutex<NativeState>,
}

#[derive(Default)]
struct NativeState {
    requests: Vec<Value>,
    previous: Vec<u8>,
    usage: Vec<(u64, u64)>,
}
impl Respond for NativeCache {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let mut canonical = body.clone();
        for message in canonical["messages"].as_array_mut().unwrap() {
            for block in message["content"].as_array_mut().unwrap() {
                block.as_object_mut().unwrap().remove("cache_control");
            }
        }
        let messages = canonical
            .as_object_mut()
            .unwrap()
            .remove("messages")
            .unwrap();
        canonical.as_object_mut().unwrap().remove("stream");
        // One synthetic token per byte. Settings precede newline-delimited messages,
        // so an appended conversation can reuse every byte of the previous input.
        let mut bytes = serde_json::to_vec(&canonical).unwrap();
        bytes.push(b'\n');
        for message in messages.as_array().unwrap() {
            bytes.extend(serde_json::to_vec(message).unwrap());
            bytes.push(b'\n');
        }
        let mut state = self.state.lock().unwrap();
        let input = bytes.len() as u64;
        let cached = state
            .previous
            .iter()
            .zip(&bytes)
            .take_while(|(a, b)| a == b)
            .count() as u64;
        let turn = state.requests.len();
        state.requests.push(body);
        state.previous = bytes;
        state.usage.push((input, cached));
        let mut frames = vec![(
            "message_start",
            json!({"type":"message_start","message":{"id":format!("reply-{turn}"),"type":"message","role":"assistant","model":"claude-sonnet-4-6","content":[],"usage":{"input_tokens":input-cached,"cache_read_input_tokens":cached,"output_tokens":0}}}),
        )];
        if turn < 3 {
            frames.push(("content_block_start",json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":format!("call-{turn}"),"name":"read","input":{},"extra":"retained"}})));
            let arguments = if turn == 1 {
                "{\"index\":".into()
            } else {
                json!({"index":turn}).to_string()
            };
            frames.push(("content_block_delta",json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":arguments}})));
        } else {
            frames.push(("content_block_start",json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"done"}})));
        }
        frames.push((
            "content_block_stop",
            json!({"type":"content_block_stop","index":0}),
        ));
        frames.push(("message_delta",json!({"type":"message_delta","delta":{"stop_reason":if turn==1 {"max_tokens"} else if turn<3 {"tool_use"} else {"end_turn"}},"usage":{"output_tokens":1}})));
        frames.push(("message_stop", json!({"type":"message_stop"})));
        let sse = frames
            .into_iter()
            .map(|(event, data)| format!("event: {event}\ndata: {data}\n\n"))
            .collect::<String>();
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(sse)
    }
}

#[tokio::test]
async fn truncated_tool_arguments_preserve_anthropic_cache_prefix_and_settings() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"data":[{"id":"claude-sonnet-4-6"}]})),
        )
        .mount(&server)
        .await;
    let cache = Arc::new(NativeCache::default());
    let responder = cache.clone();
    Mock::given(method("POST"))
        .respond_with(move |request: &Request| responder.respond(request))
        .mount(&server)
        .await;
    let directory = test_socket_dir::SplitDir::new();
    std::fs::create_dir_all(directory.path().join(config::PROJECT_CONFIG_DIR)).unwrap();
    std::fs::write(
        config::project_main_config_path(directory.path()),
        format!(
            r#"
[providers.local]
type = "anthropic"
base_url = "{}"
api_key_env = "{KEY_ENV}"
models = ["claude-sonnet-4-6"]
default_model = "claude-sonnet-4-6"
[[routing.routes.worker]]
profile = "local"
[[routing.routes.orchestrator]]
profile = "local"
[[routing.routes.planner]]
profile = "local"
[[routing.routes.reviewer]]
profile = "local"
[compaction]
context_window_tokens = 1000000
threshold = 0.99
"#,
            server.uri()
        ),
    )
    .unwrap();
    let config = Config::load(&LoadOptions {
        project_dir: Some(directory.path().into()),
        user_config_dir: Some(directory.path().join(config::PROJECT_CONFIG_DIR)),
        read_env: false,
        ..Default::default()
    })
    .unwrap();
    let bus = Arc::new(EventBus::new(1024));
    let mut receiver = bus.subscribe();
    let mut executor = ToolExecutor::new(bus.clone());
    executor.register(Arc::new(BulkRead)).unwrap();
    let runtime = compose_runtime(RuntimeComposition {
        model_catalog_dir: Some(test_support::model_catalog_dir()),
        user_config_dir: Some(directory.path().join("empty")),
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
    .with_system_prompts(contract_prompts())
    .with_compaction(config.compaction.clone());
    let run = runtime.delegate_background(
        Role::Worker,
        "read then recover".into(),
        RunConfig::default(),
    );
    let events = through_phase(&mut receiver, run, AgentRunPhase::Done).await;
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Done);
    let state = cache.state.lock().unwrap();
    assert_eq!(
        state.requests.len(),
        4,
        "truncation must not auto-retry a request or tool"
    );
    for pair in state.requests.windows(2) {
        assert_append_only(CacheProtocol::Anthropic, &pair[0], &pair[1]).unwrap();
    }
    assert_eq!(state.usage[0].1, 0);
    for pair in state.usage.windows(2) {
        assert!(
            pair[1].1 >= pair[0].0,
            "input-derived cache must reuse all previously sent input"
        );
    }
    let malformed: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Diagnostic(d) if d.code == "ToolArgumentsMalformed" => Some(d),
            _ => None,
        })
        .collect();
    assert_eq!(malformed.len(), 1);
    assert!(!events.iter().any(|event|matches!(&event.kind,EventKind::Tool(event_bus::ToolEvent::ToolStarted {call_id,..}) if call_id=="call-1")));
    let third = &state.requests[2];
    let blocks: Vec<_> = third["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|m| m["content"].as_array().unwrap())
        .collect();
    let calls: Vec<_> = blocks
        .iter()
        .filter(|b| b["type"] == "tool_use" && b["id"] == "call-1")
        .collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["input"], json!({}));
    assert!(blocks.iter().any(|b| b["type"] == "tool_result"
        && b["tool_use_id"] == "call-1"
        && b["is_error"] == true));
    let usage: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::Provider(ProviderEvent::RequestCompleted {
                input_tokens,
                cache_read_tokens,
                ..
            }) => Some((*input_tokens, *cache_read_tokens)),
            _ => None,
        })
        .collect();
    assert_eq!(
        usage, state.usage,
        "reported usage must come from the captured input, not canned warm-token fixtures"
    );
    // Anthropic has no prompt_cache_key: stable model/system/tools/settings are
    // independently checked above; no new affinity field is invented by repair.
    assert!(
        state
            .requests
            .iter()
            .all(|body| body.get("prompt_cache_key").is_none())
    );
}
