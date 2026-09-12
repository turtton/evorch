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
        run_id: "run-2".into(),
    };
    assert!(
        executor
            .validate_call(ctx.clone(), "counter".into(), "bad".into(), json!({}))
            .is_err()
    );
    let prepared = executor
        .validate_call(
            ctx,
            "counter".into(),
            "call-1".into(),
            json!({"valid":true}),
        )
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
                assert_approval_id_format(&call_id);
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

fn assert_approval_id_format(call_id: &str) {
    let attempt = call_id
        .strip_prefix("run-2:call-1:")
        .expect("run and call scope");
    assert!(!attempt.is_empty());
    assert!(attempt.bytes().all(|byte| byte.is_ascii_digit()));
    assert!(attempt.parse::<u64>().is_ok());
}

#[tokio::test]
async fn prepared_ask_first_emits_scoped_attempt_id_before_tool_started() {
    // Given: an AskFirst call with the same run and call IDs used by GUI fixtures.
    let bus = Arc::new(EventBus::new(64));
    let mut events = bus.subscribe();
    let count = Arc::new(AtomicUsize::new(0));
    let mut executor = ToolExecutor::new(bus.clone());
    executor.register(Arc::new(Counter(count.clone()))).unwrap();
    executor.set_policy(
        ApprovalPolicy::standard(ApprovalMode::OnRequest)
            .with_override("counter", PolicyDecision::Ask),
    );
    executor.set_approval_gate(ApprovalGate::new(bus.clone(), Duration::from_secs(2)));
    let validated = Arc::new(executor)
        .validate_call(
            ToolExecutionContext {
                run_id: "run-2".into(),
            },
            "counter".into(),
            "call-1".into(),
            json!({"valid":true}),
        )
        .unwrap();
    // When: authorization runs before any execution.
    let task = tokio::spawn(validated.authorize());
    let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .unwrap()
        .unwrap();
    // Then: the first event is a scoped approval request and no tool has executed.
    let EventKind::Tool(ToolEvent::ApprovalRequested { call_id, .. }) = event.kind else {
        panic!("expected approval before ToolStarted");
    };
    assert_approval_id_format(&call_id);
    assert_eq!(count.load(Ordering::SeqCst), 0);
    bus.emit(Event::new(ToolEvent::ApprovalResolved {
        call_id,
        approved: true,
    }));
    let _prepared = task.await.unwrap().unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 0);
}
