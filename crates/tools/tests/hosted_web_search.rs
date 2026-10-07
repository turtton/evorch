//! Per-call hosted search preserves the executor and the explicit Exa fallback.
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use event_bus::{EventBus, EventKind, ToolEvent};
use serde_json::json;
use tools::{
    ContentOrigin, SearchError, SearchOptions, SearchProvider, SearchResults, ToolExecutionContext,
    ToolExecutor, WebSearch,
};

struct Search {
    name: &'static str,
    result: Result<SearchResults, SearchError>,
    calls: AtomicUsize,
}

impl Search {
    fn new(name: &'static str, error: Option<SearchError>) -> Arc<Self> {
        Arc::new(Self {
            name,
            result: error.map_or_else(
                || {
                    Ok(SearchResults {
                        content: format!("Title: {name}\nURL: https://example.com/{name}"),
                        result_count: 1,
                        request_id: Some(format!("request-{name}")),
                        usage: Some(json!({"input_tokens": 7})),
                    })
                },
                Err,
            ),
            calls: AtomicUsize::new(0),
        })
    }
}

#[async_trait::async_trait]
impl SearchProvider for Search {
    fn name(&self) -> &str {
        self.name
    }
    async fn search(&self, _: &str, _: &SearchOptions) -> Result<SearchResults, SearchError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.result.clone()
    }
}

#[tokio::test]
async fn hosted_primary_uses_only_exa_for_the_narrow_fallback_matrix() {
    let cases = [
        (None, false),
        (Some(SearchError::CodexUnsupported), true),
        (Some(SearchError::HttpStatus(429)), true),
        (Some(SearchError::HttpStatus(503)), true),
        (Some(SearchError::Timeout), true),
        (Some(SearchError::HttpStatus(400)), false),
        (Some(SearchError::HttpStatus(401)), false),
        (Some(SearchError::HttpStatus(403)), false),
        (Some(SearchError::Transport("failed".into())), false),
        (Some(SearchError::Protocol("incomplete".into())), false),
        (
            Some(SearchError::ProviderRejected("model unsupported".into())),
            false,
        ),
    ];
    for (error, falls_back) in cases {
        for fallback_fails in [false, true] {
            let bus = Arc::new(EventBus::new(16));
            let mut events = bus.subscribe();
            let codex = Search::new("codex", error.clone());
            let legacy = Search::new("legacy", None);
            let tavily = Search::new("tavily", None);
            let exa = Search::new("exa", fallback_fails.then_some(SearchError::Timeout));
            let mut executor = ToolExecutor::new(bus);
            executor
                .register(Arc::new(
                    WebSearch::for_providers_with_env_lookup(
                        legacy.clone(),
                        tavily.clone(),
                        Arc::new(|_| None),
                    )
                    .with_hosted_search_fallback(exa.clone()),
                ))
                .unwrap();
            let result = Arc::new(executor)
                .validate_call(
                    ToolExecutionContext {
                        run_id: "run-a".into(),
                        thread_id: None,
                        call_id: None,
                    },
                    "web_search".into(),
                    "call-a".into(),
                    json!({"query":"query"}),
                )
                .unwrap()
                .authorize()
                .await
                .unwrap()
                .with_search_provider(codex.clone())
                .execute()
                .await
                .unwrap();
            assert_eq!(codex.calls.load(Ordering::SeqCst), 1);
            assert_eq!(exa.calls.load(Ordering::SeqCst), usize::from(falls_back));
            assert_eq!(tavily.calls.load(Ordering::SeqCst), 0);
            assert_eq!(legacy.calls.load(Ordering::SeqCst), 0);
            assert_eq!(
                result.is_error,
                error.is_some() && (!falls_back || fallback_fails)
            );
            assert_eq!(result.origin, ContentOrigin::WebUntrusted);
            let detail = result.detail.unwrap();
            assert_eq!(detail["provider"], if falls_back { "exa" } else { "codex" });
            assert_eq!(detail["used_fallback"], falls_back);
            assert_eq!(detail["fallback_attempts"], u32::from(falls_back));
            assert_eq!(detail["credential_status"], "keyed");
            if !result.is_error {
                assert_eq!(detail["result_count"], 1);
                assert_eq!(detail["usage"]["input_tokens"], 7);
            }
            assert!(matches!(events.recv().await.unwrap().kind,
                EventKind::Tool(ToolEvent::ToolStarted { run_id: Some(run), .. }) if run == "run-a"));
            assert!(matches!(events.recv().await.unwrap().kind,
                EventKind::Tool(ToolEvent::ToolCompleted { run_id: Some(run), detail: Some(_), .. }) if run == "run-a"));
        }
    }
}
