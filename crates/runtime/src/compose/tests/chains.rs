use std::collections::VecDeque;
use std::time::Duration;

use super::fallback::fixture;
use super::*;

/// Fails with the queued errors first, then succeeds.
struct FlakyClient {
    failures: Mutex<VecDeque<ProviderError>>,
    requests: Arc<Mutex<Vec<ChatRequest>>>,
}

#[async_trait]
impl ProviderClient for FlakyClient {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: false,
            tool_use: true,
            reasoning: false,
        }
    }

    async fn send(
        &self,
        _auth: &ProviderAuth,
        request: &ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        self.requests.lock().unwrap().push(request.clone());
        self.failures
            .lock()
            .unwrap()
            .pop_front()
            .map_or_else(|| Ok(response()), Err)
    }

    async fn stream(
        &self,
        _auth: &ProviderAuth,
        _request: &ChatRequest,
    ) -> Result<DeltaStream, ProviderError> {
        Err(ProviderError::Request("stream unsupported".to_string()))
    }
}

fn flaky_single_member(
    failures: Vec<ProviderError>,
) -> (RoutedModel, Arc<Mutex<Vec<ChatRequest>>>) {
    let (mut model, _) = fixture(vec![None]);
    let requests = Arc::new(Mutex::new(Vec::new()));
    model.providers.get_mut("profile-0").unwrap().client = Arc::new(FlakyClient {
        failures: Mutex::new(failures.into()),
        requests: requests.clone(),
    });
    (model, requests)
}

fn rate_limited(seconds: u64) -> ProviderError {
    ProviderError::RateLimited {
        retry_after: Some(Duration::from_secs(seconds)),
    }
}

#[tokio::test(start_paused = true)]
async fn short_retry_after_waits_once_then_retries_the_exhausted_pool() {
    // Given: the only member is rate limited with a Retry-After under the 30s limit.
    let (model, requests) = flaky_single_member(vec![rate_limited(5)]);
    let started = tokio::time::Instant::now();
    // When
    let result = complete(&model, "run").await;
    // Then: it waited for the Retry-After and the retry bypassed the cooldown.
    assert_eq!(result, Ok(response()));
    assert_eq!(requests.lock().unwrap().len(), 2);
    assert!(started.elapsed() >= Duration::from_secs(5));
}

#[tokio::test(start_paused = true)]
async fn long_retry_after_exhausts_without_waiting() {
    // Given: the Retry-After exceeds the configured wait limit.
    let (model, requests) = flaky_single_member(vec![rate_limited(60)]);
    let started = tokio::time::Instant::now();
    // When
    let result = complete(&model, "run").await;
    // Then: the cooling member is skipped and the cycle ends at once.
    assert!(result.is_err());
    assert_eq!(requests.lock().unwrap().len(), 1);
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[tokio::test(start_paused = true)]
async fn wait_happens_at_most_once_per_request() {
    // Given: the member is rate limited again after the first wait.
    let (model, requests) = flaky_single_member(vec![rate_limited(5), rate_limited(5)]);
    // When
    let result = complete(&model, "run").await;
    // Then
    assert!(result.is_err());
    assert_eq!(requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn pins_export_and_seed_a_fresh_model_for_the_same_run() {
    // Given: a run that fell back to the second member.
    let (model, _) = fixture(vec![Some(ProviderError::Timeout), None]);
    assert_eq!(complete(&model, "run-1").await, Ok(response()));
    let pins = model.route_pins("run-1");
    assert_eq!(
        pins,
        [crate::RoutePinRecord {
            chain: "worker".into(),
            position: 1,
            profile: "profile-1".into(),
            model: "model-1".into(),
        }]
    );
    // When: a restarted model is seeded with the snapshot's pins.
    let (restarted, requests) = fixture(vec![None, None]);
    restarted.restore_route_pins("run-1", &pins);
    assert_eq!(complete(&restarted, "run-1").await, Ok(response()));
    assert_eq!(complete(&restarted, "run-2").await, Ok(response()));
    // Then: the restored run resumes on its pin; a new run starts from the head.
    assert_eq!(requests[1].lock().unwrap().len(), 1);
    assert_eq!(requests[0].lock().unwrap().len(), 1);
}

#[tokio::test]
async fn replacing_the_model_carries_pins_over() {
    // Given: a switchable model whose run fell back to the second member.
    let (first, _) = fixture(vec![Some(ProviderError::Timeout), None]);
    assert_eq!(complete(&first, "run-1").await, Ok(response()));
    let switchable = SwitchableModel::new(Arc::new(first));
    let (second, requests) = fixture(vec![None, None]);
    // When: a reconfigure swaps in a new model.
    switchable.replace(Arc::new(second));
    let result = switchable
        .complete(
            &AgentInvocationContext {
                run_id: "run-1".into(),
                ..Default::default()
            },
            Role::Worker,
            &[],
            &[],
        )
        .await;
    // Then: the run stays on its pinned member.
    assert_eq!(result, Ok(response()));
    assert!(requests[0].lock().unwrap().is_empty());
    assert_eq!(requests[1].lock().unwrap().len(), 1);
}
