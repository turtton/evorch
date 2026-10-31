use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use event_bus::{Event, EventBus, ProviderEvent};
use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolSpec, Usage};
use runtime::{AgentInvocationContext, AgentModel, Role, RuntimeError};

#[derive(Debug, Clone)]
pub(crate) enum Step {
    Text(String),
    Tool {
        name: String,
        input: serde_json::Value,
    },
    /// Never answers, as a provider that hangs would.
    Hang,
    /// Holds the request until `release` is notified, after telling `entered` that it
    /// arrived, and then answers with `then`. Lets a test act while a request is in flight.
    Gated {
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        then: Box<Step>,
    },
}

pub(crate) fn text(text: &str) -> Step {
    Step::Text(text.into())
}

pub(crate) fn tool(name: &str, input: serde_json::Value) -> Step {
    Step::Tool {
        name: name.into(),
        input,
    }
}

/// Answers each request with the next scripted step, then with "done".
/// Every answer is reported on `bus` as a completed request using `tokens` tokens.
pub(crate) struct ScriptedModel {
    steps: Mutex<VecDeque<Step>>,
    requests: Mutex<Vec<Vec<Message>>>,
    bus: Mutex<Option<Arc<EventBus>>>,
    tokens: u64,
}

impl ScriptedModel {
    pub(crate) fn new(steps: impl IntoIterator<Item = Step>) -> Arc<Self> {
        Self::with_tokens(steps, 10)
    }

    pub(crate) fn with_tokens(steps: impl IntoIterator<Item = Step>, tokens: u64) -> Arc<Self> {
        Arc::new(Self {
            steps: Mutex::new(steps.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
            bus: Mutex::new(None),
            tokens,
        })
    }

    pub(crate) fn attach(&self, bus: &Arc<EventBus>) {
        *self.bus.lock().unwrap() = Some(Arc::clone(bus));
    }

    pub(crate) fn requests(&self) -> Vec<Vec<Message>> {
        self.requests.lock().unwrap().clone()
    }

    fn report(&self, invocation: &AgentInvocationContext) {
        if let Some(bus) = self.bus.lock().unwrap().as_ref() {
            bus.emit(Event::new(ProviderEvent::RequestCompleted {
                usage_reported: true,
                request_id: format!("request-{}", self.requests.lock().unwrap().len()),
                provider: "scripted".into(),
                profile: None,
                protocol: "test".into(),
                model: "scripted".into(),
                streaming: false,
                duration_ms: 1,
                input_tokens: self.tokens,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                finish_reason: "stop".into(),
                run_id: Some(invocation.run_id.clone()),
                reasoning_tokens: None,
                purpose: Some(invocation.purpose),
                service_tier: None,
            }));
        }
    }
}

#[async_trait::async_trait]
impl AgentModel for ScriptedModel {
    async fn complete(
        &self,
        invocation: &AgentInvocationContext,
        _: Role,
        messages: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.requests.lock().unwrap().push(messages.to_vec());
        let mut step = self.steps.lock().unwrap().pop_front();
        if let Some(Step::Gated {
            entered,
            release,
            then,
        }) = step
        {
            entered.notify_one();
            release.notified().await;
            step = Some(*then);
        }
        let (content, finish_reason) = match step {
            Some(Step::Hang | Step::Gated { .. }) => std::future::pending().await,
            Some(Step::Tool { name, input }) => (
                vec![ContentBlock::ToolUse {
                    id: format!("call-{}", self.requests.lock().unwrap().len()),
                    name,
                    input,
                }],
                FinishReason::ToolUse,
            ),
            Some(Step::Text(text)) => (vec![ContentBlock::Text { text }], FinishReason::Stop),
            None => (
                vec![ContentBlock::Text {
                    text: "done".into(),
                }],
                FinishReason::Stop,
            ),
        };
        self.report(invocation);
        Ok(ChatResponse {
            usage_reported: true,
            message: Message {
                role: providers::Role::Assistant,
                content,
            },
            finish_reason,
            usage: Usage::default(),
        })
    }

    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "scripted".into()
    }
}

/// Every request must send the previous one unchanged and then more: by message, and as the
/// canonical serde representation of the messages. The bytes a provider receives are checked
/// at the HTTP boundary in `tests/structured_retry_cache.rs`.
pub(crate) fn assert_prefix_kept(requests: &[Vec<Message>]) {
    let wire = |messages: &[Message]| serde_json::to_string(messages).unwrap();
    for pair in requests.windows(2) {
        assert!(pair[1].len() > pair[0].len(), "a retry only appends");
        assert_eq!(pair[1][..pair[0].len()], pair[0][..]);
        let previous = wire(&pair[0]);
        let previous = previous.trim_end_matches(']');
        assert!(
            wire(&pair[1]).starts_with(previous),
            "the sent bytes were changed by a retry"
        );
    }
}

/// The simulated user's scripted decisions as the JSON the model would return.
pub(crate) fn decision(action: &str, text: &str) -> Step {
    Step::Text(
        serde_json::json!({"action": action, "text": text, "reason": "scripted"}).to_string(),
    )
}

/// A models.dev catalog holding `api`, read from a fresh cache without network.
pub(crate) async fn catalog(api: serde_json::Value) -> Arc<catalog::ModelCatalog> {
    let dir = tempfile::tempdir().unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let data = serde_json::json!({"fetched_at": now, "api": api});
    std::fs::write(dir.path().join("models-dev.json"), data.to_string()).unwrap();
    Arc::new(
        catalog::ModelCatalog::load_or_refresh(dir.path())
            .await
            .unwrap(),
    )
}

/// A completed request in a run's usage ledger.
pub(crate) fn usage(
    id: &str,
    profile: &str,
    model: &str,
    purpose: &str,
    input_tokens: u64,
) -> storage::usage::UsageRequestRecord {
    storage::usage::UsageRequestRecord {
        request_id: id.into(),
        at_ns: 1,
        provider: profile.into(),
        profile: Some(profile.into()),
        model: model.into(),
        run_id: Some("root".into()),
        parent_run_id: None,
        role: Some("worker".into()),
        purpose: Some(purpose.into()),
        status: storage::usage::UsageStatus::Ok,
        failure: None,
        finish_reason: Some("stop".into()),
        input_tokens,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: None,
        ttft_ms: None,
        duration_ms: 1,
        cost_usd: Some(1_000.0),
        service_tier: None,
    }
}

/// Write a run database holding `events` and `requests`, as a replay leaves it.
pub(crate) fn write_run_db(
    path: &std::path::Path,
    events: &[Event],
    requests: Vec<storage::usage::UsageRequestRecord>,
) {
    let storage = storage::Storage::open(storage::StorageConfig {
        db_path: path.to_path_buf(),
        ..storage::StorageConfig::default()
    })
    .unwrap();
    let handle = storage.handle();
    for event in events {
        handle.append_event(Some("eval"), event).unwrap();
    }
    handle.record_usage_requests(requests).unwrap();
    drop(storage);
}
