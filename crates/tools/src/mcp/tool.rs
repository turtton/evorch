use super::{McpToolDefinition, McpToolRegistry};
use crate::{Permissions, Tool, ToolError, ToolExecutionContext, ToolResult};
use event_bus::event::diagnostic_codes;
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
                with_images(ToolResult::success(result.text()), &result),
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
                code: diagnostic_codes::TOOL_RESULT.into(),
                detail: detail.to_string(),
                run_id: ctx.map(|ctx| ctx.run_id.clone()),
                thread_id: ctx.and_then(|ctx| ctx.thread_id.clone()),
                call_id: ctx.and_then(|ctx| ctx.call_id.clone()),
            }));
        }
        Ok(result.with_detail(detail))
    }
}

fn with_images(mut tool_result: ToolResult, result: &super::McpToolResult) -> ToolResult {
    for content in &result.content {
        let super::types::McpToolContent::Image { data, mime_type } = content else {
            continue;
        };
        match mcp_image(data, mime_type) {
            Ok(image) => tool_result = tool_result.with_image(image),
            Err(reason) => {
                tool_result
                    .content
                    .push_str(&format!("\n[image {mime_type} omitted: {reason}]"));
            }
        }
    }
    tool_result
}

fn mcp_image(data: &str, mime_type: &str) -> Result<crate::ToolImage, String> {
    use base64::Engine;
    let format = crate::image::ImageFormat::from_media_type(mime_type)
        .ok_or_else(|| "unsupported image type".to_owned())?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|error| format!("invalid base64: {error}"))?;
    let checked = crate::image::check(format, &bytes)?;
    let path = crate::output::save_file(&bytes, format.extension()).ok();
    Ok(checked.into_tool_image(path))
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

#[cfg(test)]
mod tests {
    use base64::Engine;

    use super::super::types::McpToolContent;
    use super::*;

    #[test]
    fn valid_images_attach_and_invalid_ones_leave_a_note() {
        let mut png = std::io::Cursor::new(Vec::new());
        image::RgbImage::new(2, 2)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let result = super::super::McpToolResult {
            content: vec![
                McpToolContent::Image {
                    data: base64::engine::general_purpose::STANDARD.encode(png.into_inner()),
                    mime_type: "image/png".into(),
                },
                McpToolContent::Image {
                    data: "bm90IGFuIGltYWdl".into(),
                    mime_type: "image/png".into(),
                },
            ],
            is_error: false,
            structured_content: None,
        };

        let tool_result = with_images(ToolResult::success("shot"), &result);

        assert_eq!(tool_result.images.len(), 1);
        assert_eq!(tool_result.images[0].media_type, "image/png");
        assert!(tool_result.content.contains("[image image/png omitted:"));
    }
}
