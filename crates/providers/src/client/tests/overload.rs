use super::*;

fn overloaded() -> ProviderError {
    ProviderError::Http {
        status: 400,
        body: "server_is_overloaded: Our servers are currently overloaded. Please try again later."
            .into(),
    }
}

#[tokio::test(start_paused = true)]
async fn streaming_waits_for_backoff_and_recovers_from_overload() {
    let mut client = ScriptedClient::new(vec![
        Err(overloaded()),
        Ok(vec![text("Hello"), Err(overloaded())]),
        Ok(vec![text("Hello"), text(" world"), completed()]),
    ]);
    client.policy = crate::RetryPolicy::default();
    let bus = EventBus::new(16);
    let mut receiver = bus.subscribe();
    let auth = ProviderAuth::new("key");
    let request = sample_request();
    let future = client.send_streaming(&auth, &request, &bus);
    tokio::pin!(future);

    assert!(future.as_mut().now_or_never().is_none());
    assert_eq!(client.attempts.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_millis(249)).await;
    assert!(future.as_mut().now_or_never().is_none());
    assert_eq!(client.attempts.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_millis(1)).await;
    assert!(future.as_mut().now_or_never().is_none());
    assert_eq!(client.attempts.load(Ordering::SeqCst), 2);
    tokio::time::advance(Duration::from_millis(499)).await;
    assert!(future.as_mut().now_or_never().is_none());
    assert_eq!(client.attempts.load(Ordering::SeqCst), 2);
    tokio::time::advance(Duration::from_millis(1)).await;
    assert_eq!(future.await, Ok(sample_response()));
    assert_eq!(client.attempts.load(Ordering::SeqCst), 3);
    assert_eq!(text_deltas(&mut receiver), ["Hello", " world"]);
}

#[tokio::test(start_paused = true)]
async fn streaming_overload_exhaustion_preserves_last_error_after_three_attempts() {
    let mut client = ScriptedClient::new(vec![Err(overloaded()); 3]);
    client.policy = crate::RetryPolicy::default();
    let start = tokio::time::Instant::now();
    let result = client
        .send_streaming(
            &ProviderAuth::new("key"),
            &sample_request(),
            &EventBus::new(16),
        )
        .await;

    assert_eq!(
        result,
        Err(ProviderError::RetriesExhausted {
            attempts: 3,
            last: Box::new(overloaded()),
        })
    );
    assert_eq!(client.attempts.load(Ordering::SeqCst), 3);
    assert_eq!(start.elapsed(), Duration::from_millis(750));
}

#[tokio::test(start_paused = true)]
async fn streaming_overload_respects_no_retry_policy() {
    let mut client = ScriptedClient::new(vec![Err(overloaded())]);
    client.policy = crate::RetryPolicy::no_retry();
    let start = tokio::time::Instant::now();
    let result = client
        .send_streaming(
            &ProviderAuth::new("key"),
            &sample_request(),
            &EventBus::new(16),
        )
        .await;

    assert_eq!(
        result,
        Err(ProviderError::RetriesExhausted {
            attempts: 1,
            last: Box::new(overloaded()),
        })
    );
    assert_eq!(client.attempts.load(Ordering::SeqCst), 1);
    assert_eq!(start.elapsed(), Duration::ZERO);
}

#[tokio::test(start_paused = true)]
async fn dropping_streaming_future_cancels_overload_retry() {
    let mut client = ScriptedClient::new(vec![Err(overloaded())]);
    client.policy = crate::RetryPolicy::default();
    let bus = EventBus::new(16);
    let auth = ProviderAuth::new("key");
    let request = sample_request();
    let mut future = Box::pin(client.send_streaming(&auth, &request, &bus));
    assert!(future.as_mut().now_or_never().is_none());
    drop(future);
    tokio::time::advance(Duration::from_secs(10)).await;
    assert_eq!(client.attempts.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn streaming_does_not_retry_ordinary_bad_requests() {
    let error = ProviderError::Http {
        status: 400,
        body: "invalid_request_error: invalid model".into(),
    };
    let mut client = ScriptedClient::new(vec![Err(error.clone())]);
    client.policy = crate::RetryPolicy::default();
    let start = tokio::time::Instant::now();
    let result = client
        .send_streaming(
            &ProviderAuth::new("key"),
            &sample_request(),
            &EventBus::new(16),
        )
        .await;

    assert_eq!(result, Err(error));
    assert_eq!(client.attempts.load(Ordering::SeqCst), 1);
    assert_eq!(start.elapsed(), Duration::ZERO);
}
