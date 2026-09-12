mod support;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use event_bus::{Event, EventBus, EventKind, ToolEvent};
use providers::{ContentBlock, FinishReason};
use runtime::{AgentRuntime, Role, RunConfig};
use sandbox::{ApprovalGate, ApprovalMode, ApprovalPolicy, PolicyDecision};
use serde_json::{Value, json};
use support::{ScriptedModel, text_response, tool_response};
use tokio::sync::mpsc;
use tools::{Permissions, Tool, ToolError, ToolExecutionMode, ToolExecutor, ToolResult};

type Trace = Arc<Mutex<Vec<String>>>;

struct Probe {
    name: &'static str,
    mode: ToolExecutionMode,
    trace: Trace,
    started: mpsc::UnboundedSender<String>,
}

#[async_trait::async_trait]
impl Tool for Probe {
    fn name(&self) -> &'static str {
        self.name
    }
    fn schema(&self) -> Value {
        json!({"type":"object"})
    }
    fn permissions(&self) -> Permissions {
        Permissions::read_only()
    }
    fn execution_mode(&self) -> ToolExecutionMode {
        self.mode
    }
    async fn execute(&self, args: Value) -> Result<ToolResult, ToolError> {
        let label = args["label"].as_str().expect("label").to_owned();
        self.trace
            .lock()
            .expect("trace")
            .push(format!("start:{label}"));
        self.started.send(label.clone()).expect("receiver");
        tokio::time::sleep(Duration::from_millis(args["ms"].as_u64().unwrap_or(0))).await;
        self.trace
            .lock()
            .expect("trace")
            .push(format!("end:{label}"));
        Ok(ToolResult::success(label))
    }
}

fn call(id: &str, name: &str, ms: u64) -> ContentBlock {
    ContentBlock::ToolUse {
        id: id.into(),
        name: name.into(),
        input: json!({"label":id,"ms":ms}),
    }
}

fn model(calls: Vec<ContentBlock>) -> Arc<ScriptedModel> {
    let mut response = tool_response("unused", "read", json!({}));
    response.message.content = calls;
    Arc::new(ScriptedModel::new([
        Ok(response),
        Ok(text_response("done", FinishReason::Stop)),
    ]))
}

fn executor(bus: Arc<EventBus>) -> (ToolExecutor, Trace, mpsc::UnboundedReceiver<String>) {
    let trace = Arc::new(Mutex::new(Vec::new()));
    let (started, receiver) = mpsc::unbounded_channel();
    let mut executor = ToolExecutor::new(bus);
    for (name, mode) in [
        ("read", ToolExecutionMode::Shared),
        ("grep", ToolExecutionMode::Shared),
        ("shell", ToolExecutionMode::Exclusive),
    ] {
        executor
            .register(Arc::new(Probe {
                name,
                mode,
                trace: trace.clone(),
                started: started.clone(),
            }))
            .expect("register");
    }
    (executor, trace, receiver)
}

#[tokio::test]
async fn shared_wave_runs_concurrently_exclusive_is_barrier() {
    // Given: two half-second shared calls, an exclusive, then another shared call.
    let bus = Arc::new(EventBus::new(256));
    let (executor, trace, _receiver) = executor(bus.clone());
    let runtime = AgentRuntime::new(
        bus,
        Arc::new(executor),
        model(vec![
            call("a", "read", 500),
            call("b", "grep", 500),
            call("x", "shell", 500),
            call("c", "read", 500),
        ]),
    );
    // When: execute the batch through the real agent loop.
    let start = Instant::now();
    let run = runtime.delegate_background(Role::Worker, "batch".into(), RunConfig::default());
    runtime.wait(run).await.expect("wait");
    // Then: first wave overlaps; the exclusive separates both adjacent waves.
    let elapsed = start.elapsed();
    println!("shared/exclusive/shared elapsed: {elapsed:?} (sequential baseline 2s)");
    assert!(elapsed < Duration::from_millis(1900), "{elapsed:?}");
    let trace = trace.lock().expect("trace");
    let position = |entry| trace.iter().position(|item| item == entry).expect("entry");
    assert!(position("start:b") < position("end:a"));
    assert!(position("end:a") < position("start:x"));
    assert!(position("end:b") < position("start:x"));
    assert!(position("end:x") < position("start:c"));
}

#[tokio::test]
async fn two_shared_half_second_calls_take_one_half_second_wave() {
    // Given: exactly two half-second calls.
    let bus = Arc::new(EventBus::new(256));
    let (executor, _trace, _receiver) = executor(bus.clone());
    let runtime = AgentRuntime::new(
        bus,
        Arc::new(executor),
        model(vec![call("a", "read", 500), call("b", "grep", 500)]),
    );
    // When: measure the public runtime surface.
    let start = Instant::now();
    let run = runtime.delegate_background(Role::Worker, "batch".into(), RunConfig::default());
    runtime.wait(run).await.expect("wait");
    // Then: one 500ms wave rather than two serial waits.
    let elapsed = start.elapsed();
    println!("two Shared 500ms calls: {elapsed:?}");
    assert!(elapsed >= Duration::from_millis(500) && elapsed < Duration::from_millis(900));
}

#[tokio::test]
async fn results_returned_in_original_call_order() {
    // Given: later call completes first.
    let bus = Arc::new(EventBus::new(256));
    let (executor, trace, _receiver) = executor(bus.clone());
    let model = model(vec![call("slow", "read", 100), call("fast", "grep", 0)]);
    let runtime = AgentRuntime::new(bus, Arc::new(executor), model.clone());
    // When: run the batch.
    let run = runtime.delegate_background(Role::Worker, "batch".into(), RunConfig::default());
    runtime.wait(run).await.expect("wait");
    // Then: completion is reversed but the next model request is ordered.
    let trace = trace.lock().expect("trace").clone();
    assert!(
        trace.iter().position(|s| s == "end:fast") < trace.iter().position(|s| s == "end:slow")
    );
    let observed = model.observed().await;
    let ids: Vec<_> = observed
        .last()
        .expect("request")
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            ContentBlock::ToolResult { tool_call_id, .. } => Some(tool_call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(ids, ["slow", "fast"]);
}

#[tokio::test]
async fn all_permission_checks_happen_before_any_execution() {
    // Given: a later exclusive call requires approval, which will be denied.
    let bus = Arc::new(EventBus::new(256));
    let mut events = bus.subscribe();
    let (mut executor, trace, _receiver) = executor(bus.clone());
    executor.set_policy(
        ApprovalPolicy::standard(ApprovalMode::OnRequest)
            .with_override("shell", PolicyDecision::Ask),
    );
    executor.set_approval_gate(ApprovalGate::new(bus.clone(), Duration::from_secs(2)));
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(executor),
        model(vec![call("a", "read", 0), call("denied", "shell", 0)]),
    );
    // When: observe and resolve the real approval gate.
    let run = runtime.delegate_background(Role::Worker, "batch".into(), RunConfig::default());
    let before = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let EventKind::Tool(ToolEvent::ApprovalRequested { call_id, .. }) =
                events.recv().await.expect("event").kind
            {
                let before = trace.lock().expect("trace").clone();
                bus.emit(Event::new(ToolEvent::ApprovalResolved {
                    call_id,
                    approved: false,
                }));
                break before;
            }
        }
    })
    .await
    .expect("approval");
    runtime.wait(run).await.expect("wait");
    // Then: no tool ran before the last approval, and denial remains per-call.
    assert!(
        before.is_empty(),
        "side effects before approval: {before:?}"
    );
    assert_eq!(*trace.lock().expect("trace"), ["start:a", "end:a"]);
}

#[tokio::test]
async fn meta_op_finish_short_circuits_remaining() {
    // Given: finish between an overlapping shared wave and a forbidden tail.
    let bus = Arc::new(EventBus::new(256));
    let (executor, trace, _receiver) = executor(bus.clone());
    let finish = ContentBlock::ToolUse {
        id: "finish".into(),
        name: "finish".into(),
        input: json!({"result":"done"}),
    };
    let runtime = AgentRuntime::new(
        bus,
        Arc::new(executor),
        model(vec![
            call("a", "read", 100),
            call("b", "grep", 0),
            finish,
            call("tail", "read", 0),
        ]),
    );
    // When: run as an orchestrator with finish capability.
    let run = runtime.delegate_background(Role::Orchestrator, "batch".into(), RunConfig::default());
    runtime.wait(run).await.expect("wait");
    // Then: wave completed out of order, finish stopped the tail.
    let trace = trace.lock().expect("trace");
    assert!(!trace.iter().any(|s| s.contains("tail")));
    assert!(trace.iter().position(|s| s == "end:b") < trace.iter().position(|s| s == "end:a"));
}

#[tokio::test]
async fn snapshot_taken_before_each_write_tool_in_wave() {
    // Given: real writes interleaved with a shared wave and real snapshot storage.
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("project");
    std::fs::create_dir(&root).expect("root");
    let file = root.join("file");
    std::fs::write(&file, "before").expect("file");
    let bus = Arc::new(EventBus::new(256));
    let mut events = bus.subscribe();
    let (mut executor, trace, _receiver) = executor(bus.clone());
    executor.register(Arc::new(tools::Edit)).expect("edit");
    let edit = |id: &str, text: &str| ContentBlock::ToolUse {
        id: id.into(),
        name: "edit".into(),
        input: json!({"path":file,"new_string":text}),
    };
    let service = Arc::new(
        runtime::snapshot::SnapshotService::new(&root, &temp.path().join("snapshots"))
            .expect("service"),
    );
    let runtime = AgentRuntime::new(
        bus,
        Arc::new(executor),
        model(vec![
            edit("w1", "one"),
            call("a", "read", 100),
            call("b", "grep", 0),
            edit("w2", "two"),
        ]),
    )
    .with_snapshots(service);
    // When: run all calls and restore the last checkpoint.
    let run = runtime.delegate_background(Role::Worker, "batch".into(), RunConfig::default());
    runtime.wait(run).await.expect("wait");
    let mut checkpoints = Vec::new();
    while let Ok(Ok(event)) = tokio::time::timeout(Duration::from_millis(10), events.recv()).await {
        match event.kind {
            EventKind::Snapshot(snapshot) => checkpoints.push(snapshot.call_id),
            EventKind::Tool(ToolEvent::ToolStarted {
                tool_name, call_id, ..
            }) if tool_name == "edit" => assert_eq!(checkpoints.last(), Some(&call_id)),
            _ => {}
        }
    }
    runtime.restore_snapshot(run, false).await.expect("restore");
    // Then: each write has its own immediate predecessor snapshot.
    assert_eq!(checkpoints, ["w1", "w2"]);
    assert_eq!(std::fs::read_to_string(file).expect("file"), "one");
    let trace = trace.lock().expect("trace");
    assert!(trace.iter().position(|s| s == "end:b") < trace.iter().position(|s| s == "end:a"));
}

#[tokio::test]
async fn cancel_during_wave_finishes_cancelled() {
    // Given: two long-running calls and an exclusive tail.
    let bus = Arc::new(EventBus::new(256));
    let (executor, trace, mut started) = executor(bus.clone());
    let runtime = AgentRuntime::new(
        bus,
        Arc::new(executor),
        model(vec![
            call("a", "read", 5000),
            call("b", "grep", 5000),
            call("tail", "shell", 0),
        ]),
    );
    // When: cancel only after both shared calls have entered.
    let run = runtime.delegate_background(Role::Worker, "batch".into(), RunConfig::default());
    let both = tokio::time::timeout(Duration::from_millis(500), async {
        started.recv().await.expect("first");
        started.recv().await.expect("second");
    })
    .await;
    runtime.cancel(run).expect("cancel");
    let result = tokio::time::timeout(Duration::from_secs(2), runtime.wait(run))
        .await
        .expect("cancel completes")
        .expect("wait");
    // Then: both were active, neither continued, and the run reports cancellation.
    assert!(both.is_ok(), "second shared call never started");
    assert_eq!(result, event_bus::AgentRunPhase::Error);
    assert_eq!(trace.lock().expect("trace").len(), 2);
}
