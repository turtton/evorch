use super::*;

type PendingRequest = (
    ChatRequest,
    tokio::sync::oneshot::Sender<Result<ChatResponse, ProviderError>>,
);

struct ControlledClient(tokio::sync::mpsc::UnboundedSender<PendingRequest>);

#[async_trait]
impl ProviderClient for ControlledClient {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: false,
            tool_use: false,
            reasoning: false,
        }
    }

    fn supports_structured_output(&self) -> bool {
        true
    }

    async fn send(
        &self,
        _: &ProviderAuth,
        request: &ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.0.send((request.clone(), tx)).unwrap();
        rx.await.unwrap()
    }

    async fn stream(
        &self,
        _: &ProviderAuth,
        _: &ChatRequest,
    ) -> Result<DeltaStream, ProviderError> {
        unreachable!("non-streaming fixture")
    }
}

fn controlled_review() -> (
    crate::escalation_review::QuickModelReviewer,
    tokio::sync::mpsc::UnboundedReceiver<PendingRequest>,
) {
    let (mut model, _) = routed_model(Ok(response()), "local-model", None);
    model.agents.reviewer.models = vec![config::ChainEntryConfig::route("worker")];
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    model.providers.get_mut("local").unwrap().client = Arc::new(ControlledClient(tx));
    // Exercise the production routing, format downgrade and UI model wrapper.
    let reviewer = crate::escalation_review::QuickModelReviewer::new(Arc::new(
        SwitchableModel::new(Arc::new(model)),
    ))
    .with_timeout(std::time::Duration::from_millis(500));
    (reviewer, rx)
}

#[tokio::test(start_paused = true)]
async fn one_review_deadline_covers_structured_and_legacy_attempts() {
    use std::{task::Poll, time::Duration};

    use crate::escalation_review::ReviewError;

    let (reviewer, mut requests) = controlled_review();
    let review = reviewer.review("run-review", "pwd", "inspect directory");
    tokio::pin!(review);
    assert!(futures_util::poll!(&mut review).is_pending());
    let (mut structured, reply) = requests.recv().await.unwrap();
    assert!(structured.output_schema.is_some());
    tokio::time::advance(Duration::from_millis(300)).await;
    reply.send(Err(ProviderError::Http {
        status: 400,
        body: r#"{"error":{"code":"unsupported_parameter","param":"response_format","message":"response_format is not supported"}}"#.into(),
    })).unwrap();
    assert!(futures_util::poll!(&mut review).is_pending());
    let (legacy, _reply) = requests.recv().await.unwrap();
    structured.output_schema = None;
    assert_eq!(structured, legacy, "only the unsupported format may change");

    // Keep legacy pending across the ORIGINAL deadline. A fresh deadline for
    // fallback would incorrectly leave this future pending at 500ms.
    tokio::time::advance(Duration::from_millis(199)).await;
    assert!(futures_util::poll!(&mut review).is_pending());
    tokio::time::advance(Duration::from_millis(1)).await;
    assert_eq!(
        futures_util::poll!(&mut review),
        Poll::Ready(Err(ReviewError::Timeout))
    );
    assert!(requests.try_recv().is_err());
}

#[tokio::test(start_paused = true)]
async fn transport_timeout_never_retries_in_legacy_mode() {
    let (reviewer, mut requests) = controlled_review();
    let review = reviewer.review("run-review", "pwd", "inspect directory");
    tokio::pin!(review);
    let mut attempts = 0;
    let result = loop {
        tokio::select! {
            result = &mut review => break result,
            Some((request, reply)) = requests.recv() => {
                // Routing may retry a transient failure; it must not downgrade
                // the response format on any of those attempts.
                assert!(request.output_schema.is_some());
                attempts += 1;
                reply.send(Err(ProviderError::Timeout)).unwrap();
            }
        }
    };
    assert_eq!(result, Err(crate::escalation_review::ReviewError::Model));
    assert!(attempts > 0);
    assert!(requests.try_recv().is_err(), "no prompt-only downgrade");
}

#[tokio::test]
async fn unsupported_client_uses_existing_prompt_without_schema() {
    // Given: a provider without structured-output support and a prompt-only answer.
    let (model, requests) = routed_model(Ok(response()), "local-model", None);
    let messages = vec![Message {
        role: MessageRole::User,
        content: vec![ContentBlock::Text {
            text: "Return JSON".into(),
        }],
    }];
    // When: a schema-constrained completion is requested.
    model
        .complete_structured(
            &AgentInvocationContext::default(),
            Role::Worker,
            &messages,
            &providers::JsonSchema {
                name: "verdict".into(),
                schema: serde_json::json!({"type":"object"}),
            },
        )
        .await
        .unwrap();
    // Then: one legacy attempt preserves the prompt and never exposes tools.
    let recorded = requests.lock().unwrap();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].output_schema, None);
    assert_eq!(recorded[0].messages, messages);
    assert!(recorded[0].tools.is_empty());
}

#[tokio::test]
async fn the_invocations_attempt_log_reaches_the_provider_request_without_touching_the_wire() {
    let (model, requests) = routed_model(Ok(response()), "local-model", None);
    let messages = vec![Message {
        role: MessageRole::User,
        content: vec![ContentBlock::Text {
            text: "Return JSON".into(),
        }],
    }];
    let schema = providers::JsonSchema {
        name: "verdict".into(),
        schema: serde_json::json!({"type":"object"}),
    };
    let log = providers::AttemptLog::default();
    let logged = AgentInvocationContext {
        attempts: Some(log.clone()),
        ..AgentInvocationContext::default()
    };
    model
        .complete_structured(&logged, Role::Worker, &messages, &schema)
        .await
        .unwrap();
    model
        .complete_structured(
            &AgentInvocationContext::default(),
            Role::Worker,
            &messages,
            &schema,
        )
        .await
        .unwrap();

    assert_eq!(
        log.snapshot().len(),
        1,
        "only the logged call reached the log"
    );
    // The log is observation metadata: the two requests are otherwise the same.
    let mut recorded = requests.lock().unwrap().clone();
    assert_eq!(recorded.len(), 2);
    recorded[0].observation.as_mut().unwrap().attempts = None;
    assert_eq!(recorded[0], recorded[1]);
}
