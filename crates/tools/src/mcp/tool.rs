use super::{McpToolDefinition, McpToolRegistry};
use crate::{Permissions, Tool, ToolError, ToolResult};
use std::sync::Arc;

/// MCP HTTP adapter. MCP tool metadata declares no local resource requirements,
/// so the minimal mapping is network-only, never implicit local filesystem/spawn grants.
pub struct McpTool {
    definition: McpToolDefinition,
    registry: Arc<McpToolRegistry>,
}

impl McpTool {
    pub(super) const fn new(definition: McpToolDefinition, registry: Arc<McpToolRegistry>) -> Self {
        Self {
            definition,
            registry,
        }
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
        Ok(match self.registry.call(&self.definition, args).await {
            Ok(result) => ToolResult::success(result.text()),
            Err(error) => ToolResult::error(error.to_string()).with_detail(error.detail()),
        })
    }
}
