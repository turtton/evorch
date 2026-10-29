//! Wire-level prompt-cache contract of the retries of a structured answer.
//!
//! The simulated user and the judge ask again, within one call, when a model's answer
//! cannot be used. Each retry must send the previous request unchanged and then more, with
//! the same settings, schema and cache key, so a provider can reuse its cached prefix. The
//! requests cross the real routing and provider HTTP adapters and are read back at the
//! HTTP boundary of the offline mock, whose prompt cache is derived from the request bytes.
//! No new history-replacement boundary exists: one call is one run of at most
//! `ATTEMPTS` requests.

use std::collections::BTreeMap;
use std::sync::Arc;

use config::{Config, LoadOptions};
use eval::cost::RequestUsage;
use eval::judge::{self, Judge};
use eval::simulator::UserSimulator;
use event_bus::{EventBus, EventReceiver};
use mock_openai::cache_contract::{CacheProtocol, assert_append_only};
use mock_openai::{ScriptedResponse, StreamingMockOpenAi};
use routing::{ComposeDeps, MapEnv, factory::FactoryOptions};
use runtime::AgentModel;
use sandbox::credential::FileCredentialStore;
use serde_json::{Value, json};

const MODEL: &str = "mock-model";
const KEY_ENV: &str = "EVORCH_EVAL_STRUCTURED_RETRY_KEY";
const THREAD: &str = "thread-1";

const PROSE: &str = "Let me think about what to answer first.";
const DECISION: &str = r#"{"action":"reply","text":"go on","reason":"scripted"}"#;
const ANALYSIS: &str = r#"{"time_and_tokens":"a","detours":"b","delegation":"c","swapped_roles":"d","difference":"e"}"#;

/// The provider wire a retry crosses.
#[derive(Clone, Copy, Debug)]
enum Wire {
    /// `type = "openai-compatible"`: Chat Completions.
    ChatCompletions,
    /// `type = "openai"`: the official Responses API.
    Responses,
}

impl Wire {
    const fn provider_type(self) -> &'static str {
        match self {
            Self::ChatCompletions => "openai-compatible",
            Self::Responses => "openai",
        }
    }

    const fn path(self) -> &'static str {
        match self {
            Self::ChatCompletions => "/v1/chat/completions",
            Self::Responses => "/v1/responses",
        }
    }

    const fn protocol(self) -> CacheProtocol {
        match self {
            Self::ChatCompletions => CacheProtocol::OpenAi,
            Self::Responses => CacheProtocol::Codex,
        }
    }

    const fn history_key(self) -> &'static str {
        match self {
            Self::ChatCompletions => "messages",
            Self::Responses => "input",
        }
    }

    /// The structured-output schema of a captured request.
    fn schema(self, request: &Value) -> &Value {
        match self {
            Self::ChatCompletions => &request["response_format"]["json_schema"],
            Self::Responses => &request["text"]["format"],
        }
    }
}

struct Harness {
    _directory: tempfile::TempDir,
    wire: Wire,
    mock: StreamingMockOpenAi,
    bus: Arc<EventBus>,
    model: Arc<dyn AgentModel>,
}

/// A real routed model for `wire`, bound to the worker and orchestrator slots the
/// simulated user and the judge are invoked through, in front of the offline mock.
fn harness(wire: Wire, script: Vec<ScriptedResponse>) -> Harness {
    let directory = tempfile::tempdir().unwrap();
    let mock = StreamingMockOpenAi::spawn_with_prompt_cache_models(script, vec![MODEL.into()]);
    std::fs::create_dir_all(directory.path().join(config::PROJECT_CONFIG_DIR)).unwrap();
    std::fs::write(
        config::project_main_config_path(directory.path()),
        format!(
            r#"
[providers.local]
type = "{provider_type}"
base_url = "{base_url}"
api_key_env = "{KEY_ENV}"
models = ["{MODEL}"]
default_model = "{MODEL}"
[[routing.routes.orchestrator]]
profile = "local"
[[routing.routes.worker]]
profile = "local"
"#,
            provider_type = wire.provider_type(),
            base_url = mock.base_url(),
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
    let model = runtime::compose::compose_routed_model(
        &config,
        ComposeDeps {
            credential_store: Arc::new(
                FileCredentialStore::open(directory.path().join("credentials")).unwrap(),
            ),
            event_bus: Some(Arc::clone(&bus)),
            env: Arc::new(MapEnv::from_iter([(KEY_ENV, "offline-test-key")])),
            catalog: model::ModelCatalog::new(),
            factory: FactoryOptions::default(),
        },
        Some(test_support::model_catalog_dir()),
    )
    .unwrap();
    Harness {
        _directory: directory,
        wire,
        mock,
        bus,
        model,
    }
}

fn text(fragment: &str) -> ScriptedResponse {
    ScriptedResponse::text_stream("answer", MODEL, [fragment])
}

/// An answer without any text, as a model that calls a tool instead of answering.
fn tool_call() -> ScriptedResponse {
    ScriptedResponse::tool_call(
        "answer",
        MODEL,
        0,
        "call-1",
        "shell",
        ["{\"command\":\"ls\"}"],
    )
}

impl Harness {
    /// The requests that reached the provider, in order.
    fn requests(&self) -> Vec<Value> {
        self.mock
            .recorded_requests()
            .into_iter()
            .filter(|request| request.method == "POST" && request.path == self.wire.path())
            .map(|request| request.body)
            .collect()
    }

    /// Check the contract on the three captured requests and the usage the mock derived
    /// from their bytes. `growth` is the number of history items each retry appended.
    fn assert_contract(&self, usage: &[RequestUsage], growth: [usize; 2]) {
        let wire = self.wire;
        let requests = self.requests();
        assert_eq!(requests.len(), 3, "every attempt reaches the provider");
        for pair in requests.windows(2) {
            assert_append_only(wire.protocol(), &pair[0], &pair[1])
                .expect("a retry keeps the sent prefix and the settings");
        }
        let lengths: Vec<usize> = requests
            .iter()
            .map(|request| request[wire.history_key()].as_array().unwrap().len())
            .collect();
        assert_eq!(
            [lengths[1] - lengths[0], lengths[2] - lengths[1]],
            growth,
            "{lengths:?}"
        );
        // The schema is requested every time, and the model and cache key do not move.
        let schema = wire.schema(&requests[0]);
        assert!(
            schema.to_string().contains("additionalProperties"),
            "a schema is requested: {schema}"
        );
        let cache_key = &requests[0]["prompt_cache_key"];
        // The Responses wire carries the run id as its affinity key; Chat Completions has none.
        match wire {
            Wire::Responses => assert!(
                cache_key.as_str().is_some_and(|key| key.contains(THREAD)),
                "{cache_key}"
            ),
            Wire::ChatCompletions => assert!(cache_key.is_null(), "{cache_key}"),
        }
        for request in &requests {
            assert_eq!(wire.schema(request), schema);
            assert_eq!(request["model"], requests[0]["model"]);
            assert_eq!(&request["prompt_cache_key"], cache_key);
        }
        // Usage derived from the input bytes: nothing cached at first, then at least the
        // whole previous input is reused, and the new tail is not.
        assert_eq!(usage.len(), 3, "{usage:?}");
        assert_eq!(usage[0].cache_read_tokens, 0);
        for pair in usage.windows(2) {
            assert!(pair[0].input_tokens > 0);
            assert!(
                pair[1].cache_read_tokens >= pair[0].input_tokens,
                "the whole previous input is reused: {usage:?}"
            );
            assert!(
                pair[1].cache_read_tokens < pair[1].input_tokens,
                "{usage:?}"
            );
        }
        assert_eq!(self.mock.remaining_scripts(), 0);
    }
}

/// The usage of the simulated user's requests, as the event bus saw them.
fn simulator_usage(receiver: &mut EventReceiver) -> Vec<RequestUsage> {
    let run_id = format!("user-simulator:{THREAD}");
    receiver
        .drain_pending_snapshot()
        .iter()
        .filter_map(|event| RequestUsage::from_event(&event.kind))
        .filter(|request| request.run_id.as_deref() == Some(run_id.as_str()))
        .collect()
}

async fn simulator_retries(wire: Wire) {
    // Given: a prose answer, an answer with no text, then a usable decision.
    let harness = harness(wire, vec![text(PROSE), tool_call(), text(DECISION)]);
    let mut receiver = harness.bus.subscribe();
    let simulator = UserSimulator::new(Arc::clone(&harness.model), THREAD);

    // When: the next step is requested.
    let decision = simulator.decide(&[], &[], None).await.unwrap();

    // Then: the third attempt succeeds, and each retry only appended: the prose and a
    // correction request, then (the tool call having no text) the correction request alone.
    assert_eq!(decision.text, "go on");
    let usage = simulator_usage(&mut receiver);
    harness.assert_contract(&usage, [2, 1]);
}

async fn analysis_retries(wire: Wire) {
    // Given: two unusable answers of different kinds, then a usable analysis.
    let harness = harness(wire, vec![text(PROSE), tool_call(), text(ANALYSIS)]);
    let judge = Judge::new(Arc::clone(&harness.model), Arc::clone(&harness.bus), THREAD);

    // When: the process analysis is requested.
    let judged = judge
        .analyze(&json!({"baseline": {}, "candidate": {}}))
        .await;

    // Then: the analysis is used, and the retries only appended.
    assert_eq!(judged.result.unwrap().difference, "e");
    harness.assert_contract(&judged.usage, [2, 1]);
}

async fn comparison_retries(wire: Wire) {
    // Given: prose, then an answer missing an aspect (rejected after parsing), then a
    // complete comparison.
    let aspects = judge::aspects(false);
    let verdict = json!({"winner": "A", "reason": "r"});
    let complete: BTreeMap<_, _> = aspects
        .iter()
        .map(|aspect| (*aspect, verdict.clone()))
        .collect();
    let mut partial = complete.clone();
    partial.remove("efficiency");
    let harness = harness(
        wire,
        vec![
            text(PROSE),
            text(&serde_json::to_string(&partial).unwrap()),
            text(&serde_json::to_string(&complete).unwrap()),
        ],
    );
    let judge = Judge::new(Arc::clone(&harness.model), Arc::clone(&harness.bus), THREAD);

    // When: the pairwise comparison is requested.
    let judged = judge.compare(&json!({"A": {}, "B": {}}), &aspects).await;

    // Then: the comparison is used, and each retry appended the rejected answer and a request.
    assert_eq!(judged.result.unwrap().len(), aspects.len());
    harness.assert_contract(&judged.usage, [2, 2]);
}

#[tokio::test]
async fn simulator_retries_keep_the_chat_completions_prefix() {
    simulator_retries(Wire::ChatCompletions).await;
}

#[tokio::test]
async fn simulator_retries_keep_the_responses_prefix() {
    simulator_retries(Wire::Responses).await;
}

#[tokio::test]
async fn analysis_retries_keep_the_chat_completions_prefix() {
    analysis_retries(Wire::ChatCompletions).await;
}

#[tokio::test]
async fn analysis_retries_keep_the_responses_prefix() {
    analysis_retries(Wire::Responses).await;
}

#[tokio::test]
async fn comparison_retries_keep_the_chat_completions_prefix() {
    comparison_retries(Wire::ChatCompletions).await;
}

#[tokio::test]
async fn comparison_retries_keep_the_responses_prefix() {
    comparison_retries(Wire::Responses).await;
}
