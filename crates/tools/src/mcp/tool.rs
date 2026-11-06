use super::{
    McpToolDefinition, McpToolRegistry, McpToolTransport, registry::McpToolBinding,
    types::FrozenMcpTool,
};
use crate::{Permissions, Tool, ToolError, ToolExecutionContext, ToolResult};
use event_bus::event::diagnostic_codes;
use event_bus::{DiagnosticEvent, DiagnosticSeverity, Event, EventBus};
use std::sync::Arc;

/// Explicit public binding plus the original remote definition. Startup consent
/// and the executor's per-call scope/permission gate remain separate authorities.
pub struct McpTool {
    binding: McpToolBinding,
    transport: McpToolTransport,
    registry: Arc<McpToolRegistry>,
    event_bus: Option<Arc<EventBus>>,
}

impl McpTool {
    pub(super) fn new(definition: McpToolDefinition, registry: Arc<McpToolRegistry>) -> Self {
        Self {
            binding: McpToolBinding::Legacy(definition),
            transport: McpToolTransport::Http,
            registry,
            event_bus: None,
        }
    }

    pub(super) fn new_frozen(
        tool: FrozenMcpTool,
        registry: Arc<McpToolRegistry>,
        transport: McpToolTransport,
    ) -> Self {
        Self {
            binding: McpToolBinding::Frozen(Arc::new(tool)),
            transport,
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
        let (result, detail, severity) = match self.registry.call(&self.binding, args).await {
            Ok((result, mut detail)) => {
                if result.is_error
                    && let Some(metadata) = detail.as_object_mut()
                {
                    metadata.insert("class".into(), serde_json::json!("tool_rejected"));
                }
                (
                    with_images(
                        if result.is_error {
                            ToolResult::error(result_text(&result))
                        } else {
                            ToolResult::success(result_text(&result))
                        },
                        &result,
                    ),
                    detail,
                    if result.is_error {
                        DiagnosticSeverity::Error
                    } else {
                        DiagnosticSeverity::Info
                    },
                )
            }
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
        // Bound once at the return boundary. The executor preserves an existing
        // output artifact instead of rewriting a previously returned result.
        Ok(crate::output::limit_result(result.with_detail(detail)))
    }
}

// Metadata is untrusted too: bound and JSON-quote it (including control bytes).
const MEDIA_METADATA_BYTES: usize = 512;
const MAX_MEDIA_ARTIFACTS: usize = 8;
fn metadata(value: &str) -> String {
    let mut end = value.len().min(MEDIA_METADATA_BYTES);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    let truncated = end < value.len();
    let value = serde_json::to_string(&value[..end]).expect("string serialization");
    if truncated {
        format!("{value}…")
    } else {
        value
    }
}

fn result_text(result: &super::McpToolResult) -> String {
    result_text_with(result, |bytes, extension| {
        crate::output::save_file(bytes, extension).ok()
    })
}

/// Never fetch a resource URI. Unsupported payloads become local artifacts, not
/// silent omissions or model-visible base64. Only the already-returned payload
/// is written; failure to retain it is explicit and never exposes an OS error.
fn result_text_with(
    result: &super::McpToolResult,
    mut save: impl FnMut(&[u8], &str) -> Option<std::path::PathBuf>,
) -> String {
    use super::types::McpToolContent;
    use base64::Engine;
    let mut saved = 0;
    let mut retain = |bytes: &[u8], extension: &str| {
        if saved == MAX_MEDIA_ARTIFACTS {
            return "artifact omitted: per-result artifact limit reached".to_owned();
        }
        saved += 1;
        save(bytes, extension).map_or_else(
            || "artifact unavailable".into(),
            |path| format!("artifact: {}", path.display()),
        )
    };
    let mut parts = Vec::new();
    for content in &result.content {
        match content {
            McpToolContent::Text { text } => parts.push(text.clone()),
            McpToolContent::Image { .. } => {} // handled by with_images
            McpToolContent::Audio { data, mime_type } => {
                let retained = match base64::engine::general_purpose::STANDARD.decode(data) {
                    Ok(bytes) => format!("{} bytes; {}", bytes.len(), retain(&bytes, "bin")),
                    Err(_) => "invalid base64; omitted".into(),
                };
                parts.push(format!(
                    "[audio mimeType={}; playback unsupported; {retained}]",
                    metadata(mime_type)
                ));
            }
            McpToolContent::ResourceLink { uri, name } => {
                let retained = if uri.len() > MEDIA_METADATA_BYTES
                    || name.len() > MEDIA_METADATA_BYTES
                {
                    let reference = serde_json::to_vec(&serde_json::json!({"uri":uri,"name":name}))
                        .expect("JSON value serialization");
                    format!("; {}", retain(&reference, "json"))
                } else {
                    String::new()
                };
                parts.push(format!(
                    "[resource_link name={} uri={}; reference only; not fetched{retained}]",
                    metadata(name),
                    metadata(uri)
                ));
            }
            McpToolContent::Resource { resource } => {
                let uri = resource
                    .get("uri")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let mime = resource
                    .get("mimeType")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unspecified");
                let mut retained = Vec::new();
                if let Some(text) = resource.get("text").and_then(serde_json::Value::as_str) {
                    retained.push(format!(
                        "text {} bytes; {}",
                        text.len(),
                        retain(text.as_bytes(), "txt")
                    ));
                }
                if let Some(blob) = resource.get("blob").and_then(serde_json::Value::as_str) {
                    retained.push(
                        match base64::engine::general_purpose::STANDARD.decode(blob) {
                            Ok(bytes) => {
                                format!("binary {} bytes; {}", bytes.len(), retain(&bytes, "bin"))
                            }
                            Err(_) => "invalid base64; omitted".into(),
                        },
                    );
                }
                if retained.is_empty() {
                    retained.push("no supported embedded payload".into());
                }
                parts.push(format!(
                    "[embedded resource uri={} mimeType={}; {}; URI not fetched]",
                    metadata(uri),
                    metadata(mime),
                    retained.join("; ")
                ));
            }
        }
    }
    if let Some(content) = &result.structured_content {
        parts.push(serde_json::to_string(content).expect("JSON value serialization"));
    }
    parts.join("\n")
}

fn with_images(mut tool_result: ToolResult, result: &super::McpToolResult) -> ToolResult {
    let mut images = 0;
    let mut omitted = 0;
    for content in &result.content {
        let super::types::McpToolContent::Image { data, mime_type } = content else {
            continue;
        };
        if images == crate::output::MAX_RESULT_IMAGES {
            omitted += 1;
            continue;
        }
        images += 1;
        match mcp_image(data, mime_type) {
            Ok(image) => tool_result = tool_result.with_image(image),
            Err(reason) => {
                tool_result.content.push_str(&format!(
                    "\n[image {} omitted: {reason}]",
                    metadata(mime_type)
                ));
            }
        }
    }
    if omitted > 0 {
        tool_result.content.push_str(&format!(
            "\n[{omitted} more images omitted: per-result image limit reached]"
        ));
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
        self.binding.public_name()
    }
    fn description(&self) -> &str {
        self.binding
            .definition()
            .description
            .as_deref()
            .unwrap_or("")
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::Value::Object(self.binding.definition().input_schema.clone())
    }
    fn permissions(&self) -> Permissions {
        self.transport.permissions()
    }
    fn requires_scope_gate(&self) -> bool {
        true
    }
    /// The server owns the schema and what `null` means in it.
    fn reads_null_as_omitted(&self) -> bool {
        false
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
        assert!(
            tool_result.content.contains("image/png") && tool_result.content.contains("omitted:")
        );
    }
    #[test]
    fn mixed_results_keep_structured_json_and_reference_unsupported_media_without_fetching() {
        let result: super::super::McpToolResult = serde_json::from_value(serde_json::json!({
            "content": [
                {"type":"text","text":"before"},
                {"type":"audio","data":"YWJj","mimeType":"audio/wav"},
                {"type":"resource","resource":{"uri":"https://untrusted.invalid/never-fetch","mimeType":"text/plain","text":"embedded body"}},
                {"type":"resource","resource":{"uri":"file:///never-read","blob":"AAE="}},
                {"type":"resource_link","uri":"https://untrusted.invalid/link","name":"reference"}
            ],
            "structuredContent":{"answer":42},"isError":true
        })).unwrap();
        let mut saved = Vec::new();
        let text = result_text_with(&result, |bytes, extension| {
            saved.push((bytes.to_vec(), extension.to_owned()));
            Some(std::path::PathBuf::from(format!(
                "/fixture/{}.{}",
                saved.len(),
                extension
            )))
        });
        assert!(text.starts_with("before\n"));
        assert!(text.contains("audio/wav") && text.contains("3 bytes"));
        assert!(text.contains("artifact: /fixture/1.bin"));
        assert!(text.contains("embedded resource") && text.contains("text/plain"));
        assert!(text.contains("https://untrusted.invalid/never-fetch"));
        assert!(text.contains("file:///never-read"));
        assert!(text.contains("reference only; not fetched"));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(text.lines().last().unwrap()).unwrap(),
            serde_json::json!({"answer":42})
        );
        assert_eq!(
            saved,
            vec![
                (b"abc".to_vec(), "bin".into()),
                (b"embedded body".to_vec(), "txt".into()),
                (vec![0, 1], "bin".into())
            ]
        );
        assert!(!text.contains("YWJj") && !text.contains("embedded body"));
    }

    #[test]
    fn invalid_media_and_artifact_failure_are_explicit_and_metadata_is_bounded() {
        let result: super::super::McpToolResult = serde_json::from_value(serde_json::json!({
            "content":[
                {"type":"audio","data":"!invalid!","mimeType":"audio/wav"},
                {"type":"resource","resource":{"uri":"あ".repeat(10000),"text":"body"}},
                {"type":"resource_link","uri":"https://invalid/\ncontrol","name":"ref"}
            ]
        }))
        .unwrap();
        let mut saved = 0;
        let text = result_text_with(&result, |_, _| {
            saved += 1;
            None
        });
        assert_eq!(
            saved, 1,
            "invalid audio must not be retained as valid media"
        );
        assert!(text.contains("invalid base64; omitted"));
        assert!(text.contains("artifact unavailable"));
        assert!(text.contains("\\ncontrol"));
        assert!(!text.contains("body"));
        assert!(text.len() < 2048, "large metadata must not flood the model");
    }

    #[test]
    fn media_artifact_count_is_bounded_and_overflow_is_visible() {
        let result = super::super::McpToolResult {
            content: (0..MAX_MEDIA_ARTIFACTS + 1)
                .map(|_| McpToolContent::Audio {
                    data: "YWJj".into(),
                    mime_type: "audio/wav".into(),
                })
                .collect(),
            is_error: false,
            structured_content: None,
        };
        let mut saves = 0;
        let text = result_text_with(&result, |_, _| {
            saves += 1;
            Some("/fixture/media".into())
        });
        assert_eq!(saves, MAX_MEDIA_ARTIFACTS);
        assert!(text.contains("artifact limit reached"));
    }
}
