use std::sync::Arc;

use async_trait::async_trait;
use event_bus::EventBus;
use sandbox::DirectSandbox;
use tools::{Permissions, Tool, ToolError, ToolExecutionMode, ToolExecutor, ToolResult};

fn executor_with_web_tools() -> ToolExecutor {
    ToolExecutor::with_standard_tools(
        Arc::new(EventBus::new(16)),
        Arc::new(DirectSandbox::new_unchecked()),
    )
    .with_web_tools()
    .expect("web tools should initialize")
}

// Given: all tools registered by the standard and web-tool builders / When: their execution modes are queried / Then: read-like tools are Shared and mutating tools are Exclusive
#[test]
fn registered_tools_report_execution_mode() {
    let executor = executor_with_web_tools();
    let expected = [
        ("read", ToolExecutionMode::Shared),
        ("edit", ToolExecutionMode::Exclusive),
        ("grep", ToolExecutionMode::Shared),
        ("shell", ToolExecutionMode::Exclusive),
        ("git_diff", ToolExecutionMode::Shared),
        ("web_search", ToolExecutionMode::Shared),
        ("web_fetch", ToolExecutionMode::Shared),
    ];

    for (tool_name, mode) in expected {
        assert_eq!(executor.tool_execution_mode(tool_name), mode, "{tool_name}");
    }
}

struct AmbiguousTool;

#[async_trait]
impl Tool for AmbiguousTool {
    fn name(&self) -> &'static str {
        "ambiguous"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "additionalProperties": false})
    }

    fn permissions(&self) -> Permissions {
        Permissions::read_only()
    }

    async fn execute(&self, _args: serde_json::Value) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::success("ambiguous"))
    }
}

// Given: an unregistered name and a registered tool with no explicit classification / When: execution modes are queried / Then: both default to Exclusive
#[test]
fn unregistered_or_ambiguous_defaults_exclusive() {
    let mut executor = ToolExecutor::new(Arc::new(EventBus::new(16)));
    executor
        .register(Arc::new(AmbiguousTool))
        .expect("ambiguous tool should register");

    assert_eq!(
        executor.tool_execution_mode("missing"),
        ToolExecutionMode::Exclusive
    );
    assert_eq!(
        executor.tool_execution_mode("ambiguous"),
        ToolExecutionMode::Exclusive
    );
}
