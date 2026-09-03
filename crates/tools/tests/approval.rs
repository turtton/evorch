//! 承認方針を適用する ToolExecutor の統合テスト。

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use event_bus::{Event, EventBus, EventKind, EventReceiver, ToolEvent};
use sandbox::{ApprovalGate, ApprovalMode, ApprovalPolicy};
use tools::{Permissions, Tool, ToolError, ToolExecutionContext, ToolExecutor, ToolResult};

struct CountingTool {
    name: &'static str,
    permissions: Permissions,
    calls: Arc<AtomicUsize>,
    outcome: Result<ToolResult, ToolError>,
}

#[async_trait]
impl Tool for CountingTool {
    fn name(&self) -> &'static str {
        self.name
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "additionalProperties": false})
    }

    fn permissions(&self) -> Permissions {
        self.permissions
    }

    async fn execute(&self, _args: serde_json::Value) -> Result<ToolResult, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.outcome.clone()
    }
}

fn executor_with_tool(
    bus: Arc<EventBus>,
    permissions: Permissions,
    outcome: Result<ToolResult, ToolError>,
) -> (ToolExecutor, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut executor = ToolExecutor::new(bus);
    executor
        .register(Arc::new(CountingTool {
            name: "fake",
            permissions,
            calls: Arc::clone(&calls),
            outcome,
        }))
        .expect("テストツールを登録できるはずです");
    (executor, calls)
}

fn tool_event(event: Event) -> ToolEvent {
    let EventKind::Tool(event) = event.kind else {
        panic!("Tool イベントを期待しました");
    };
    event
}

async fn next(receiver: &mut EventReceiver) -> ToolEvent {
    tool_event(receiver.recv().await.expect("イベントを受信できるはずです"))
}

fn spawn_responder(
    bus: Arc<EventBus>,
    mut receiver: EventReceiver,
    approved: bool,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let event = receiver.recv().await.expect("承認要求を受信できるはずです");
            if let EventKind::Tool(ToolEvent::ApprovalRequested { call_id, .. }) = event.kind {
                bus.emit(Event::new(ToolEvent::ApprovalResolved {
                    call_id,
                    approved,
                }));
                return;
            }
        }
    })
}

fn configure_gate(executor: &mut ToolExecutor, bus: Arc<EventBus>, mode: ApprovalMode) {
    executor
        .set_policy(ApprovalPolicy::standard(mode))
        .set_approval_gate(ApprovalGate::new(bus, Duration::from_secs(1)));
}

// Given: 全許可方針の読み取りツール / When: 実行 / Then: 承認イベントなしで開始・完了しツールが1回動く
#[tokio::test]
async fn auto_allow_executes_without_approval_events() {
    let bus = Arc::new(EventBus::new(16));
    let mut receiver = bus.subscribe();
    let (executor, calls) = executor_with_tool(
        Arc::clone(&bus),
        Permissions::read_only(),
        Ok(ToolResult::success("成功")),
    );

    let result = executor
        .execute(
            &ToolExecutionContext {
                run_id: "run-1".to_string(),
            },
            "fake",
            "call-1",
            serde_json::json!({}),
        )
        .await;

    assert!(!result.expect("自動許可されるはずです").is_error);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(matches!(
        next(&mut receiver).await,
        ToolEvent::ToolStarted { .. }
    ));
    assert!(matches!(
        next(&mut receiver).await,
        ToolEvent::ToolCompleted {
            is_error: false,
            ..
        }
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(50), receiver.recv())
            .await
            .is_err()
    );
}

// Given: 実行前承認が必要なプロセスツールと拒否応答 / When: 実行 / Then: ツールを呼ばず拒否イベント後にエラー完了する
#[tokio::test]
async fn ask_first_denial_prevents_execution() {
    let bus = Arc::new(EventBus::new(16));
    let mut observer = bus.subscribe();
    let responder = spawn_responder(Arc::clone(&bus), bus.subscribe(), false);
    let (mut executor, calls) = executor_with_tool(
        Arc::clone(&bus),
        Permissions::process(),
        Ok(ToolResult::success("未実行")),
    );
    configure_gate(&mut executor, Arc::clone(&bus), ApprovalMode::OnRequest);

    let error = executor
        .execute(
            &ToolExecutionContext {
                run_id: "run-1".to_string(),
            },
            "fake",
            "call-2",
            serde_json::json!({}),
        )
        .await
        .expect_err("拒否されるはずです");
    responder.await.expect("応答タスクが完了するはずです");

    assert!(matches!(error, ToolError::ExecutionDenied { .. }));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(matches!(
        next(&mut observer).await,
        ToolEvent::ToolStarted { .. }
    ));
    assert!(matches!(
        next(&mut observer).await,
        ToolEvent::ApprovalRequested { .. }
    ));
    assert!(matches!(
        next(&mut observer).await,
        ToolEvent::ApprovalResolved {
            approved: false,
            ..
        }
    ));
    assert!(matches!(
        next(&mut observer).await,
        ToolEvent::ExecutionDenied { .. }
    ));
    assert!(matches!(
        next(&mut observer).await,
        ToolEvent::ToolCompleted { is_error: true, .. }
    ));
}

// Given: 実行前承認が必要なツールと承認応答 / When: 実行 / Then: 承認解決後に1回実行して完了する
#[tokio::test]
async fn ask_first_approval_executes_once() {
    let bus = Arc::new(EventBus::new(16));
    let mut observer = bus.subscribe();
    let responder = spawn_responder(Arc::clone(&bus), bus.subscribe(), true);
    let (mut executor, calls) = executor_with_tool(
        Arc::clone(&bus),
        Permissions::process(),
        Ok(ToolResult::success("成功")),
    );
    configure_gate(&mut executor, Arc::clone(&bus), ApprovalMode::OnRequest);

    executor
        .execute(
            &ToolExecutionContext {
                run_id: "run-1".to_string(),
            },
            "fake",
            "call-3",
            serde_json::json!({}),
        )
        .await
        .expect("承認後に成功するはずです");
    responder.await.expect("応答タスクが完了するはずです");

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    for expected in ["開始", "要求", "解決", "完了"] {
        let event = next(&mut observer).await;
        let matched = matches!(
            (expected, event),
            ("開始", ToolEvent::ToolStarted { .. })
                | ("要求", ToolEvent::ApprovalRequested { .. })
                | ("解決", ToolEvent::ApprovalResolved { approved: true, .. })
                | (
                    "完了",
                    ToolEvent::ToolCompleted {
                        is_error: false,
                        ..
                    }
                )
        );
        assert!(matched, "{expected}イベントの順序が不正です");
    }
}

// Given: Never 方針または承認ゲートなしの要承認ツール / When: 実行 / Then: 承認要求なしで閉じて拒否する
#[tokio::test]
async fn ask_without_available_approval_path_fails_closed() {
    for (call_id, mode) in [
        ("never", ApprovalMode::Never),
        ("no-gate", ApprovalMode::OnRequest),
    ] {
        let bus = Arc::new(EventBus::new(16));
        let mut observer = bus.subscribe();
        let (mut executor, calls) = executor_with_tool(
            Arc::clone(&bus),
            Permissions::process(),
            Ok(ToolResult::success("未実行")),
        );
        executor.set_policy(ApprovalPolicy::standard(mode));

        let error = executor
            .execute(
                &ToolExecutionContext {
                    run_id: "run-1".to_string(),
                },
                "fake",
                call_id,
                serde_json::json!({}),
            )
            .await
            .expect_err("閉じて拒否されるはずです");

        assert!(matches!(error, ToolError::ExecutionDenied { .. }));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(matches!(
            next(&mut observer).await,
            ToolEvent::ToolStarted { .. }
        ));
        assert!(matches!(
            next(&mut observer).await,
            ToolEvent::ExecutionDenied { .. }
        ));
        assert!(matches!(
            next(&mut observer).await,
            ToolEvent::ToolCompleted { is_error: true, .. }
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), observer.recv())
                .await
                .is_err()
        );
    }
}

// Given: 初回失敗するツールと失敗時承認 / When: 承認または拒否 / Then: 承認時だけ1回再試行し最終完了イベントは1件になる
#[tokio::test]
async fn on_failure_retries_once_only_when_approved() {
    for (approved, expected_calls) in [(true, 2), (false, 1)] {
        let bus = Arc::new(EventBus::new(16));
        let mut observer = bus.subscribe();
        let responder = spawn_responder(Arc::clone(&bus), bus.subscribe(), approved);
        let original = ToolError::Io {
            detail: "初回失敗".to_string(),
        };
        let (mut executor, calls) = executor_with_tool(
            Arc::clone(&bus),
            Permissions::process(),
            Err(original.clone()),
        );
        configure_gate(&mut executor, Arc::clone(&bus), ApprovalMode::OnFailure);

        let error = executor
            .execute(
                &ToolExecutionContext {
                    run_id: "run-1".to_string(),
                },
                "fake",
                "call-failure",
                serde_json::json!({}),
            )
            .await
            .expect_err("失敗結果が返るはずです");
        responder.await.expect("応答タスクが完了するはずです");

        assert_eq!(error, original);
        assert_eq!(calls.load(Ordering::SeqCst), expected_calls);
        let mut completed = 0;
        for _ in 0..4 {
            if matches!(next(&mut observer).await, ToolEvent::ToolCompleted { .. }) {
                completed += 1;
            }
        }
        assert_eq!(completed, 1, "ToolCompleted は1件だけであるべきです");
    }
}
