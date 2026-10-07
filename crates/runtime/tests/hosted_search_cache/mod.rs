//! Chat → hosted search → chat exercises the actual Codex HTTP/SSE path.
use std::sync::{Arc, Mutex};

use event_bus::{
    AgentRunPhase, CacheComparison, EventBus, EventKind, ProviderEvent, RequestPurpose,
};
use mock_openai::cache_contract::{CacheProtocol, assert_append_only};
use runtime::{AgentRuntime, Role, RunConfig};
use serde_json::{Value, json};
use tools::{SearchError, SearchOptions, SearchProvider, SearchResults, ToolExecutor, WebSearch};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

mod support;

struct UnexpectedFallback;
#[async_trait::async_trait]
impl SearchProvider for UnexpectedFallback {
    fn name(&self) -> &str {
        "unexpected"
    }
    async fn search(&self, _: &str, _: &SearchOptions) -> Result<SearchResults, SearchError> {
        panic!("normal hosted search must never reach the legacy chain")
    }
}

#[derive(Default)]
struct Trace {
    requests: Vec<(Value, u64, u64)>,
    prompts: Vec<(Value, Value, Vec<u8>)>,
    chats: usize,
}

impl Trace {
    // Independent input-derived cache oracle: every byte is one synthetic token.
    // Retain settings and input item order, while excluding transport-only flags.
    fn usage(&mut self, request: &Value) -> (u64, u64) {
        let mut settings = request.clone();
        let input = settings.as_object_mut().unwrap().remove("input").unwrap();
        for key in ["stream", "stream_options"] {
            settings.as_object_mut().unwrap().remove(key);
        }
        let mut bytes = serde_json::to_vec(&settings).unwrap();
        bytes.push(b'\n');
        for item in input.as_array().unwrap() {
            bytes.extend(serde_json::to_vec(item).unwrap());
            bytes.push(b'\n');
        }
        let key = request["prompt_cache_key"].clone();
        let model = request["model"].clone();
        let cached = self
            .prompts
            .iter()
            .filter(|(m, k, _)| m == &model && k == &key)
            .map(|(_, _, old)| old.iter().zip(&bytes).take_while(|(a, b)| a == b).count() as u64)
            .max()
            .unwrap_or(0);
        let input = bytes.len() as u64;
        self.prompts.push((model, key, bytes));
        self.requests.push((request.clone(), input, cached));
        (input, cached)
    }
}

fn sse(events: Vec<Value>) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(
        events
            .into_iter()
            .map(|event| {
                format!(
                    "event: {}\ndata: {event}\n\n",
                    event["type"].as_str().unwrap()
                )
            })
            .collect::<String>(),
        "text/event-stream",
    )
}

fn reply(hosted: bool, chat: usize, input: u64, cached: u64) -> ResponseTemplate {
    let usage = json!({"input_tokens":input, "output_tokens":4, "input_tokens_details":{"cached_tokens":cached}});
    let mut completed = json!({"type":"response.completed", "response":{"id":format!("response-{hosted}-{chat}"), "status":"completed", "usage":usage}});
    if hosted {
        completed["response"]["output"] = json!([
            {"id":"search", "type":"web_search_call", "status":"completed", "action":{"type":"search", "query":"query"}},
            {"id":"answer", "type":"message", "role":"assistant", "status":"completed", "content":[{
                "type":"output_text", "text":"A supported answer.", "annotations":[{
                    "type":"url_citation", "url":"https://example.com/source", "title":"Source", "start_index":0, "end_index":1
                }]
            }]}
        ]);
        return sse(vec![completed]);
    }
    if chat < 2 {
        let id = format!("tool-{chat}");
        let arguments = json!({"query":format!("query {chat}"), "max_results":1}).to_string();
        sse(vec![
            json!({"type":"response.output_item.added", "output_index":0, "item":{"id":id, "type":"function_call", "call_id":id, "name":"web_search", "arguments":""}}),
            json!({"type":"response.function_call_arguments.delta", "item_id":id, "output_index":0, "delta":arguments}),
            json!({"type":"response.function_call_arguments.done", "item_id":id, "output_index":0, "arguments":arguments}),
            json!({"type":"response.output_item.done", "output_index":0, "item":{"id":id, "type":"function_call", "call_id":id, "name":"web_search", "arguments":arguments, "status":"completed"}}),
            completed,
        ])
    } else {
        sse(vec![
            json!({"type":"response.output_text.delta", "item_id":"final", "content_index":0, "delta":"Research complete"}),
            completed,
        ])
    }
}

#[tokio::test]
async fn codex_hosted_search_preserves_chat_wire_prefix_affinity_and_cache_baseline() {
    let server = MockServer::start().await;
    let bus = Arc::new(EventBus::new(256));
    let mut events = bus.subscribe();
    let trace = Arc::new(Mutex::new(Trace::default()));
    let record = trace.clone();
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .respond_with(move |request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let hosted = body["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["type"] == "web_search");
            let mut trace = record.lock().unwrap();
            let (input, cached) = trace.usage(&body);
            let chat = trace.chats;
            if !hosted {
                trace.chats += 1;
            }
            reply(hosted, chat, input, cached)
        })
        .expect(5)
        .mount(&server)
        .await;
    let (model, _directory) = support::model(&server, bus.clone()).await;
    let mut executor = ToolExecutor::new(bus.clone());
    executor
        .register(Arc::new(
            WebSearch::for_providers(Arc::new(UnexpectedFallback), Arc::new(UnexpectedFallback))
                .with_hosted_search_fallback(Arc::new(UnexpectedFallback)),
        ))
        .unwrap();
    let runtime = AgentRuntime::new(bus, Arc::new(executor), model);
    runtime.set_web_tools_enabled(true);
    let run = runtime.delegate_background(
        Role::WebResearcher,
        "Research the topic and check a second source.".into(),
        RunConfig::default(),
    );
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Done);
    let events = super::through_phase(&mut events, run, AgentRunPhase::Done).await;
    let trace = trace.lock().unwrap();
    assert_eq!(trace.requests.len(), 5);
    let chats: Vec<_> = trace.requests.iter().step_by(2).collect();
    for pair in chats.windows(2) {
        assert_append_only(CacheProtocol::Codex, &pair[0].0, &pair[1].0).unwrap();
        assert_eq!(
            pair[1].2, pair[0].1,
            "input-derived cache must retain the whole previous prompt"
        );
    }
    for (body, _, _) in trace.requests.iter().skip(1).step_by(2) {
        assert_eq!(
            body["tools"],
            json!([{"type":"web_search", "external_web_access":true}])
        );
        assert_eq!(body["input"].as_array().unwrap().len(), 1);
        assert_eq!(body["model"], chats[0].0["model"]);
    }
    let completed: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Provider(ProviderEvent::RequestCompleted {
                request_id,
                purpose,
                ..
            }) => Some((request_id, purpose)),
            _ => None,
        })
        .collect();
    assert_eq!(completed.len(), 5);
    assert_eq!(
        completed
            .iter()
            .filter(|(_, purpose)| **purpose == Some(RequestPurpose::WebSearch))
            .count(),
        2
    );
    let chat_ids: Vec<_> = completed
        .iter()
        .filter(|(_, purpose)| **purpose == Some(RequestPurpose::Agent))
        .map(|(id, _)| *id)
        .collect();
    let observations: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Provider(ProviderEvent::CacheReuseObserved {
                request_id,
                comparison,
                ..
            }) => Some((request_id, comparison)),
            _ => None,
        })
        .collect();
    assert_eq!(
        observations.len(),
        3,
        "search must not observe or replace the chat cache baseline"
    );
    assert!(
        matches!(observations[2].1, CacheComparison::Compared { previous_request_id, .. } if previous_request_id == chat_ids[1])
    );
    assert!(events.iter().all(
        |event| !matches!(&event.kind, EventKind::Diagnostic(d) if d.code == "CacheRegression")
    ));
}
