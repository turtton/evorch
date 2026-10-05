use super::*;
use providers::{CompactionResult, Compactor};

struct OfficialClient {
    requests: Arc<Mutex<Vec<ChatRequest>>>,
    fail: bool,
}

#[async_trait]
impl Compactor for OfficialClient {
    async fn compact(&self, request: &ChatRequest) -> Result<CompactionResult, ProviderError> {
        self.requests.lock().unwrap().push(request.clone());
        if self.fail {
            return Err(ProviderError::Request("compaction failed".into()));
        }
        Ok(CompactionResult {
            encrypted_content: "opaque".into(),
            usage: Usage {
                input_tokens: 42,
                output_tokens: 7,
                ..Usage::default()
            },
        })
    }
}

#[async_trait]
impl ProviderClient for OfficialClient {
    fn compactor(&self) -> Option<&dyn Compactor> {
        Some(self)
    }
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tool_use: true,
            reasoning: true,
        }
    }
    async fn send(
        &self,
        _auth: &ProviderAuth,
        _request: &ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        panic!("compact must not call send")
    }
    async fn stream(
        &self,
        _auth: &ProviderAuth,
        _request: &ChatRequest,
    ) -> Result<DeltaStream, ProviderError> {
        panic!("compact must not call stream")
    }
}

#[tokio::test]
async fn compaction_preserves_routing_generation_tools_and_observation() {
    let (mut model, requests) = routed_model(Ok(response()), "gpt-4o+fast", None);
    model.providers.get_mut("local").unwrap().client = Arc::new(OfficialClient {
        requests: requests.clone(),
        fail: false,
    });
    let messages = vec![Message {
        role: MessageRole::User,
        content: vec![ContentBlock::Text {
            text: "history".into(),
        }],
    }];
    let tools = vec![ToolSpec {
        name: "read".into(),
        description: "Read".into(),
        input_schema: serde_json::json!({"type":"object"}),
    }];
    let invocation = AgentInvocationContext {
        run_id: "run-compact".into(),
        ..Default::default()
    };
    let result = model
        .compact_context(&invocation, Role::Worker, &messages, &tools)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.encrypted_content, "opaque");
    assert_eq!(result.usage.input_tokens, 42);
    let recorded = requests.lock().unwrap();
    assert_eq!(recorded.len(), 1);
    let request = &recorded[0];
    assert_eq!(request.model, "gpt-4o");
    assert_eq!(request.service_tier, Some(providers::ServiceTier::Priority));
    assert_eq!(request.temperature, Some(0.25));
    assert_eq!(request.max_tokens, Some(321));
    assert_eq!(request.messages, messages);
    assert_eq!(request.tools, tools);
    assert_eq!(
        request.observation.as_ref().unwrap().run_id,
        invocation.run_id
    );
    assert!(request.output_schema.is_none());
}

#[tokio::test]
async fn compaction_respects_explicit_preference_and_propagates_failure_without_retry() {
    let (mut model, ordinary) = routed_model(Ok(response()), "default", None);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut preferred = profile("preferred+fast", &["preferred+fast"]);
    preferred.name = "preferred".into();
    model.providers.insert(
        "preferred".into(),
        ComposedProvider {
            profile: preferred,
            client: Arc::new(OfficialClient {
                requests: requests.clone(),
                fail: true,
            }),
            auth: ProviderAuth::new("unused"),
        },
    );
    let invocation = AgentInvocationContext {
        model_preference: Some(crate::ModelPreference {
            profile: "preferred".into(),
            model: None,
            reasoning_effort: None,
        }),
        ..Default::default()
    };
    assert!(
        model
            .compact_context(&invocation, Role::Worker, &[], &[])
            .await
            .is_err()
    );
    assert!(ordinary.lock().unwrap().is_empty());
    let recorded = requests.lock().unwrap();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].model, "preferred");
    assert_eq!(recorded[0].temperature, None);
}

#[tokio::test]
async fn unsupported_provider_returns_none_without_sending() {
    let (model, requests) = routed_model(Ok(response()), "default", None);
    assert_eq!(
        model
            .compact_context(&AgentInvocationContext::default(), Role::Worker, &[], &[])
            .await
            .unwrap(),
        None
    );
    assert!(requests.lock().unwrap().is_empty());
}
