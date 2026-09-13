use super::{McpToolDefinition, McpToolRegistry};
use crate::{Permissions, Tool, ToolError, ToolExecutionContext, ToolResult};
use event_bus::{DiagnosticEvent, DiagnosticSeverity, Event, EventBus};
use std::sync::Arc;

/// MCP HTTP adapter. MCP tool metadata declares no local resource requirements,
/// so the minimal mapping is network-only, never implicit local filesystem/spawn grants.
pub struct McpTool {
    definition: McpToolDefinition,
    registry: Arc<McpToolRegistry>,
    event_bus: Option<Arc<EventBus>>,
}

impl McpTool {
    pub(super) const fn new(definition: McpToolDefinition, registry: Arc<McpToolRegistry>) -> Self {
        Self {
            definition,
            registry,
            event_bus: None,
        }
    }

    /// Inject the diagnostic emitter; correlation comes from execution context.
    pub fn with_event_bus(mut self, event_bus: Arc<EventBus>) -> Self {
        self.event_bus = Some(event_bus);
        self
    }

    async fn invoke(
        &self,
        args: serde_json::Value,
        ctx: Option<&ToolExecutionContext>,
    ) -> Result<ToolResult, ToolError> {
        let (result, detail, severity) = match self.registry.call(&self.definition, args).await {
            Ok((result, detail)) => (
                ToolResult::success(result.text()),
                detail,
                DiagnosticSeverity::Info,
            ),
            Err(error) => (
                ToolResult::error(error.to_string()),
                error.detail(),
                DiagnosticSeverity::Error,
            ),
        };
        if let Some(bus) = &self.event_bus {
            bus.emit(Event::new(DiagnosticEvent {
                source: "mcp".into(),
                severity,
                code: "tool_result".into(),
                detail: detail.to_string(),
                run_id: ctx.map(|ctx| ctx.run_id.clone()),
                thread_id: ctx.and_then(|ctx| ctx.thread_id.clone()),
                call_id: ctx.and_then(|ctx| ctx.call_id.clone()),
            }));
        }
        Ok(result.with_detail(detail))
    }
}

#[async_trait::async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.definition.name
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::Value::Object(self.definition.input_schema.clone())
    }
    fn permissions(&self) -> Permissions {
        Permissions::network()
    }
    fn requires_scope_gate(&self) -> bool {
        true
    }
    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult, ToolError> {
        self.invoke(args, None).await
    }

    async fn execute_with_context(
        &self,
        ctx: &ToolExecutionContext,
        args: serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        self.invoke(args, Some(ctx)).await
    }
}
