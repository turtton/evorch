use super::*;
use browser::agent::observation::ExecutionStamps;
struct Probe;
#[async_trait::async_trait]
impl Tool for Probe {
    fn name(&self) -> &str {
        "stamp_probe"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object"})
    }
    fn permissions(&self) -> Permissions {
        Permissions::read_only()
    }
    async fn execute(&self, _: serde_json::Value) -> Result<ToolResult, ToolError> {
        let input = ExecutionStamps;
        let captured = input.for_creation("run").unwrap();
        Ok(ToolResult::success(captured.incarnation.clone()))
    }
}
#[tokio::test]
async fn direct_and_prepared_calls_receive_the_immutable_stamp_without_schema_changes() {
    let mut executor = ToolExecutor::new(Arc::new(EventBus::new(16)));
    executor.register(Arc::new(Probe)).unwrap();
    let specs = executor.tool_specs();
    let executor = Arc::new(executor.with_browser_execution("run".into(), "incarnation".into()));
    assert_eq!(specs, executor.tool_specs());
    let ctx = ToolExecutionContext {
        run_id: "run".into(),
        ..Default::default()
    };
    let direct = executor
        .execute(&ctx, "stamp_probe", "a", serde_json::json!({}))
        .await
        .unwrap();
    let prepared = executor
        .validate_call(ctx, "stamp_probe".into(), "b".into(), serde_json::json!({}))
        .unwrap()
        .authorize()
        .await
        .unwrap()
        .execute()
        .await
        .unwrap();
    assert_eq!(direct.content, "incarnation");
    assert_eq!(prepared.content, "incarnation");
    assert!(ExecutionStamps.for_creation("run").is_none());
}
