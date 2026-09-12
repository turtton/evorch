use event_bus::{Event, EventBus, EventKind, ToolEvent};
use sandbox::{ApprovalGate, ApprovalMode, ApprovalPolicy, PolicyDecision};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tools::{Permissions, Tool, ToolError, ToolExecutionContext, ToolExecutor, ToolResult};

struct Counter(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl Tool for Counter {
    fn name(&self) -> &'static str {
        "counter"
    }
    fn schema(&self) -> Value {
        json!({"type":"object", "required":["valid"]})
    }
    fn permissions(&self) -> Permissions {
        Permissions::read_only()
    }
    async fn execute(&self, _: Value) -> Result<ToolResult, ToolError> {
        if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
            Ok(ToolResult::error("first attempt"))
        } else {
            Ok(ToolResult::success("second attempt"))
        }
    }
}

#[tokio::test]
async fn prepared_on_failure_defers_approval_until_failed_execution() {
    // Given: an OnFailure policy and a tool that fails its first attempt.
    let bus = Arc::new(EventBus::new(64));
    let count = Arc::new(AtomicUsize::new(0));
    let mut executor = ToolExecutor::new(bus.clone());
    executor
        .register(Arc::new(Counter(count.clone())))
        .expect("register");
    executor.set_policy(
        ApprovalPolicy::standard(ApprovalMode::OnFailure)
            .with_override("counter", PolicyDecision::Ask),
    );
    executor.set_approval_gate(ApprovalGate::new(bus.clone(), Duration::from_secs(2)));
    let executor = Arc::new(executor);
    let ctx = ToolExecutionContext {
        run_id: "run".into(),
    };
    assert!(
        executor
            .validate_call(ctx.clone(), "counter".into(), "bad".into(), json!({}))
            .is_err()
    );
    let prepared = executor
        .validate_call(ctx, "counter".into(), "call".into(), json!({"valid":true}))
        .expect("validate")
        .authorize()
        .await
        .expect("authorize");
    assert_eq!(count.load(Ordering::SeqCst), 0);
    let mut events = bus.subscribe();
    let responder = tokio::spawn(async move {
        loop {
            if let EventKind::Tool(ToolEvent::ApprovalRequested { call_id, .. }) =
                events.recv().await.expect("event").kind
            {
                assert_eq!(count.load(Ordering::SeqCst), 1);
                bus.emit(Event::new(ToolEvent::ApprovalResolved {
                    call_id,
                    approved: true,
                }));
                break;
            }
        }
    });
    // When: execute the token after preflight.
    let result = prepared.execute().await.expect("execute");
    responder.await.expect("responder");
    // Then: failure-time approval still enables the retry.
    assert!(!result.is_error);
    assert_eq!(result.content, "second attempt");
}
