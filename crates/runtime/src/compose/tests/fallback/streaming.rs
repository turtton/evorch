use super::*;

struct StreamingClient(StubClient);

#[async_trait]
impl ProviderClient for StreamingClient {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            ..self.0.capabilities()
        }
    }

    fn retry_policy(&self) -> providers::retry::RetryPolicy {
        providers::retry::RetryPolicy {
            initial_delay: std::time::Duration::ZERO,
            ..Default::default()
        }
    }

    async fn send(&self, _: &ProviderAuth, _: &ChatRequest) -> Result<ChatResponse, ProviderError> {
        panic!("streaming must not call send")
    }

    async fn stream(
        &self,
        _: &ProviderAuth,
        request: &ChatRequest,
    ) -> Result<DeltaStream, ProviderError> {
        self.0.requests.lock().unwrap().push(request.clone());
        let response = self.0.result.clone()?;
        Ok(Box::pin(futures_util::stream::iter([Ok(
            providers::StreamEvent::Completed { response },
        )])))
    }
}

#[tokio::test]
async fn streaming_falls_back_after_provider_retries_are_exhausted() {
    // Given
    let (mut model, requests) = fixture(vec![Some(ProviderError::Timeout), None]);
    for (index, error) in [Some(ProviderError::Timeout), None].into_iter().enumerate() {
        model
            .providers
            .get_mut(&format!("profile-{index}"))
            .unwrap()
            .client = Arc::new(StreamingClient(StubClient {
            result: error.map_or_else(|| Ok(response()), Err),
            requests: requests[index].clone(),
        }));
    }
    let bus = EventBus::new(32);
    let invocation = AgentInvocationContext {
        run_id: "session".into(),
        category: None,
        model_preference: None,
    };
    // When
    let result = model
        .complete_streaming(&invocation, Role::Worker, &[], &[], &bus)
        .await;
    // Then
    assert_eq!(result, Ok(response()));
    assert_eq!(requests[0].lock().unwrap().len(), 3);
    assert_eq!(requests[1].lock().unwrap().len(), 1);
}
