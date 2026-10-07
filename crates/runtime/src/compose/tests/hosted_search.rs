use super::*;
use providers::{
    HostedWebSearch, HostedWebSearchCitation, HostedWebSearchError, HostedWebSearchRequest,
    HostedWebSearchResponse,
};
use tools::search::SearchOptions;

#[derive(Clone)]
struct SearchClient {
    chat: StubClient,
    searches: Arc<Mutex<Vec<(String, HostedWebSearchRequest)>>>,
}

#[async_trait]
impl ProviderClient for SearchClient {
    fn capabilities(&self) -> ProviderCapabilities {
        self.chat.capabilities()
    }
    fn hosted_web_search(&self) -> Option<&dyn HostedWebSearch> {
        Some(self)
    }
    async fn send(
        &self,
        auth: &ProviderAuth,
        request: &ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        self.chat.send(auth, request).await
    }
    async fn stream(
        &self,
        auth: &ProviderAuth,
        request: &ChatRequest,
    ) -> Result<DeltaStream, ProviderError> {
        self.chat.stream(auth, request).await
    }
}

#[async_trait]
impl HostedWebSearch for SearchClient {
    async fn search(
        &self,
        auth: &ProviderAuth,
        request: &HostedWebSearchRequest,
    ) -> Result<HostedWebSearchResponse, HostedWebSearchError> {
        self.searches
            .lock()
            .unwrap()
            .push((auth.api_key.clone(), request.clone()));
        let usage = Usage {
            input_tokens: 21,
            output_tokens: 4,
            ..Usage::default()
        };
        if let Some(sink) = &request.usage_sink {
            sink(usage);
        }
        Ok(HostedWebSearchResponse {
            response_id: Some("hosted-response".into()),
            text: "Search answer".into(),
            usage,
            citations: (0..4)
                .map(|index| HostedWebSearchCitation {
                    url: format!("https://example.com/{}", index / 2),
                    title: format!("Title {index}"),
                })
                .collect(),
        })
    }
}

fn enable_search(
    model: &mut RoutedModel,
    profile: &str,
) -> Arc<Mutex<Vec<(String, HostedWebSearchRequest)>>> {
    let searches = Arc::new(Mutex::new(Vec::new()));
    model.providers.get_mut(profile).unwrap().client = Arc::new(SearchClient {
        chat: StubClient {
            result: Ok(response()),
            requests: Arc::new(Mutex::new(Vec::new())),
        },
        searches: searches.clone(),
    });
    searches
}

fn invocation(run: &str, profile: &str, effort: &str) -> AgentInvocationContext {
    AgentInvocationContext {
        run_id: run.into(),
        model_preference: Some(crate::ModelPreference {
            profile: profile.into(),
            model: None,
            reasoning_effort: Some(effort.into()),
        }),
        ..Default::default()
    }
}

#[tokio::test]
async fn concurrent_searches_retain_each_runs_profile_auth_model_and_effort() {
    let (mut model, _) = super::fallback::fixture_with_efforts(vec![None, None], &[]);
    let recordings = [
        enable_search(&mut model, "profile-0"),
        enable_search(&mut model, "profile-1"),
    ];
    let usage = Arc::new(Mutex::new(Vec::new()));
    let sink = {
        let usage = usage.clone();
        Arc::new(move |value| usage.lock().unwrap().push(value))
    };
    let a = model
        .web_search_provider(
            &invocation("run-a", "profile-0", "low"),
            Role::Worker,
            &[],
            sink.clone(),
        )
        .unwrap()
        .unwrap();
    let b = model
        .web_search_provider(
            &invocation("run-b", "profile-1", "high"),
            Role::Worker,
            &[],
            sink,
        )
        .unwrap()
        .unwrap();
    let options = SearchOptions {
        max_results: Some(1),
    };
    let (a, b) = tokio::join!(a.search("alpha", &options), b.search("beta", &options));
    for result in [a.unwrap(), b.unwrap()] {
        assert_eq!(result.result_count, 1);
        assert_eq!(result.content.matches("URL:").count(), 1);
        assert_eq!(result.request_id.as_deref(), Some("hosted-response"));
        assert_eq!(result.usage.unwrap()["input_tokens"], 21);
    }
    assert_eq!(usage.lock().unwrap().len(), 2);
    for (index, (run, effort, query)) in [("run-a", "low", "alpha"), ("run-b", "high", "beta")]
        .into_iter()
        .enumerate()
    {
        let records = recordings[index].lock().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].0, format!("secret-{index}"));
        let request = &records[0].1;
        assert_eq!(request.model, format!("model-{index}"));
        assert_eq!(request.reasoning_effort.as_deref(), Some(effort));
        assert_eq!(request.query, query);
        assert_eq!(request.observation.as_ref().unwrap().run_id, run);
        assert_eq!(
            request.observation.as_ref().unwrap().purpose,
            event_bus::RequestPurpose::WebSearch
        );
    }
}

#[tokio::test]
async fn search_uses_the_chat_fallback_pin_and_its_reasoning_effort() {
    let (mut model, _) = super::fallback::fixture_with_efforts(
        vec![Some(ProviderError::Timeout), None],
        &[Some("high"), Some("low")],
    );
    let searches = enable_search(&mut model, "profile-1");
    let invocation = AgentInvocationContext {
        run_id: "fallback-run".into(),
        ..Default::default()
    };
    let specs = [ToolSpec {
        name: "web_search".into(),
        description: "search".into(),
        input_schema: serde_json::json!({}),
    }];
    assert!(
        model
            .web_search_provider(&invocation, Role::Worker, &specs, Arc::new(|_| {}))
            .unwrap()
            .is_none()
    );
    model
        .complete(&invocation, Role::Worker, &[], &specs)
        .await
        .unwrap();
    model
        .web_search_provider(&invocation, Role::Worker, &specs, Arc::new(|_| {}))
        .unwrap()
        .unwrap()
        .search("after fallback", &SearchOptions::default())
        .await
        .unwrap();
    let records = searches.lock().unwrap();
    assert_eq!(records[0].0, "secret-1");
    assert_eq!(records[0].1.model, "model-1");
    assert_eq!(records[0].1.reasoning_effort.as_deref(), Some("low"));
}

#[tokio::test]
async fn project_replacement_cannot_change_an_existing_invocation_capability() {
    let (mut old, _) = routed_model(Ok(response()), "old-model", None);
    let searches = enable_search(&mut old, "local");
    let switchable = super::super::SwitchableModel::new(Arc::new(old));
    let captured = switchable.invocation_snapshot().unwrap();
    let (new, _) = routed_model(Ok(response()), "new-model", None);
    switchable.replace(Arc::new(new));
    let invocation = invocation("project-run", "local", "medium");
    captured
        .web_search_provider(&invocation, Role::Worker, &[], Arc::new(|_| {}))
        .unwrap()
        .unwrap()
        .search("old project", &SearchOptions::default())
        .await
        .unwrap();
    assert_eq!(searches.lock().unwrap()[0].1.model, "old-model");
    assert!(
        switchable
            .invocation_snapshot()
            .unwrap()
            .web_search_provider(&invocation, Role::Worker, &[], Arc::new(|_| {}))
            .unwrap()
            .is_none()
    );
}
