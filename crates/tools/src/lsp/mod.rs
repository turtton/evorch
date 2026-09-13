//! One-shot sandboxed LSP diagnostics. Each invocation opens a fresh document version.

mod client;
mod protocol;

use crate::{Permissions, Tool, ToolError, ToolExecutionContext, ToolResult};
use event_bus::{DiagnosticEvent, DiagnosticSeverity, Event, EventBus};
use sandbox::{CommandSpec, Sandbox};
use serde::Deserialize;
use std::{path::PathBuf, sync::Arc, time::Duration};

#[derive(Debug, thiserror::Error)]
pub enum LspError {
    #[error("LSP session failed: {0}")]
    Session(#[from] sandbox::SessionError),
    #[error("LSP I/O failed")]
    Io(#[from] std::io::Error),
    #[error("invalid LSP frame or diagnostics")]
    Protocol,
    #[error("LSP server closed its output")]
    Closed,
    #[error("LSP server request failed")]
    Server,
    #[error("LSP operation timed out")]
    Timeout,
    #[error("LSP worker failed")]
    Worker,
    #[error("LSP requires an absolute regular-file path")]
    Path,
}

/// Constructor-injected server configuration; register explicitly with ToolExecutor.
pub struct LspDiagnostics {
    sandbox: Arc<dyn Sandbox>,
    command: CommandSpec,
    language: String,
    timeout: Duration,
    event_bus: Option<Arc<EventBus>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    path: PathBuf,
}

impl LspDiagnostics {
    pub fn new(sandbox: Arc<dyn Sandbox>, command: CommandSpec, language: String) -> Self {
        Self {
            sandbox,
            command,
            language,
            timeout: Duration::from_secs(10),
            event_bus: None,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Context has no emitter or call ID; inject the bus without changing runtime wiring.
    pub fn with_event_bus(mut self, event_bus: Arc<EventBus>) -> Self {
        self.event_bus = Some(event_bus);
        self
    }

    async fn run(
        &self,
        args: Args,
        ctx: Option<&ToolExecutionContext>,
    ) -> Result<ToolResult, LspError> {
        if !args.path.is_absolute() {
            return Err(LspError::Path);
        }
        let mut client =
            client::Connection::spawn(self.sandbox.clone(), self.command.clone()).await?;
        let batch = tokio::time::timeout(self.timeout, client.open(&args.path, &self.language))
            .await
            .map_err(|_| LspError::Timeout)??;
        let content = batch.render(&args.path)?;
        let detail = serde_json::json!({"file":args.path,"count":batch.diagnostics.len(),"codes":batch.diagnostics.iter().filter_map(|d| d.code.as_ref()).collect::<Vec<_>>()});
        if let Some(bus) = &self.event_bus {
            let severity = if batch
                .diagnostics
                .iter()
                .any(|d| matches!(d.severity, Some(protocol::Severity::Error)))
            {
                DiagnosticSeverity::Error
            } else if batch
                .diagnostics
                .iter()
                .any(|d| matches!(d.severity, Some(protocol::Severity::Warning)))
            {
                DiagnosticSeverity::Warning
            } else {
                DiagnosticSeverity::Info
            };
            bus.emit(Event::new(DiagnosticEvent {
                source: "lsp".into(),
                severity,
                code: "publish_diagnostics".into(),
                detail: detail.to_string(),
                run_id: ctx.map(|ctx| ctx.run_id.clone()),
                thread_id: ctx.and_then(|ctx| ctx.thread_id.clone()),
                call_id: None,
            }));
        }
        tokio::time::timeout(self.timeout, client.shutdown())
            .await
            .map_err(|_| LspError::Timeout)??;
        Ok(ToolResult::success(content).with_detail(detail))
    }

    async fn invoke(
        &self,
        args: serde_json::Value,
        ctx: Option<&ToolExecutionContext>,
    ) -> Result<ToolResult, ToolError> {
        let args = serde_json::from_value(args).map_err(|_| ToolError::InvalidArgs {
            detail: "expected an LSP file path".into(),
        })?;
        Ok(match self.run(args, ctx).await {
            Ok(result) => result,
            Err(error) => ToolResult::error(error.to_string()),
        })
    }
}

#[async_trait::async_trait]
impl Tool for LspDiagnostics {
    fn name(&self) -> &'static str {
        "lsp_diagnostics"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"path":{"type":"string","minLength":1}},"required":["path"],"additionalProperties":false})
    }
    fn permissions(&self) -> Permissions {
        Permissions {
            fs_read: true,
            fs_write: false,
            process_spawn: true,
            network: false,
        }
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
