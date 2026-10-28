//! The `run_js` tool (ADR 0039): JavaScript that calls the agent's own tools.
//!
//! The program runs in a sandboxed `evorch js-worker` process. Each
//! `tools.<name>(args)` becomes a request over [`protocol`] that this module
//! forwards to the [`RunJsHost`] the runtime attaches to the execution context,
//! so role limits, path boundaries and approvals stay the runtime's job. The
//! model receives only what the program prints or returns, plus a short summary.

pub mod description;
pub mod normalize;
pub mod protocol;
mod serve;
mod summary;
#[cfg(test)]
mod tests;
pub mod typings;
pub mod worker;

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::time::Instant;

use crate::error::ToolError;
use crate::executor::ToolExecutionContext;
use crate::result::ToolResult;
use crate::tool::{Permissions, Tool, ToolExecutionMode};
use protocol::HostMessage;
pub use worker::WorkerLauncher;

pub const NAME: &str = "run_js";
/// Tools a program may call at most (before role limits). Delegation, questions,
/// MCP and network tools are out of scope for the first stage.
pub const CALLABLE_TOOLS: [&str; 6] = ["read", "grep", "git_diff", "edit", "write", "shell"];

/// One call made by the program, for the host to run.
#[derive(Debug, Clone)]
pub struct ChildCall {
    /// The `run_js` call that contains this one.
    pub parent_call_id: String,
    /// 1-based, in the order the program made the calls.
    pub seq: u32,
    pub name: String,
    pub args: Value,
    /// Time left of the `run_js` call; the host must not outlast it.
    pub remaining: Duration,
}

/// What the host reports about one call.
#[derive(Debug, Clone, PartialEq)]
pub struct ChildOutcome {
    /// The value `tools.<name>()` resolves to, or the message it rejects with.
    pub result: Result<Value, String>,
    pub is_error: bool,
    /// Paths the call names (read, grep, edit, write).
    pub paths: Vec<String>,
    /// Paths the call changed.
    pub written: Vec<String>,
    /// Why the call failed, in one line, for the summary the model always gets.
    pub failure: Option<String>,
    /// A post-edit hook warning on this call, in one line (the warning text itself
    /// is untrusted repository content and stays in the child call's own result).
    pub warning: Option<String>,
}

/// The runtime side of `run_js`: which tools the program may call and how they
/// run. Attached to [`ToolExecutionContext::run_js_host`].
#[async_trait::async_trait]
pub trait RunJsHost: Send + Sync + std::fmt::Debug {
    /// Names the program may call, as offered in the description.
    fn tool_names(&self) -> Vec<String>;
    fn settings(&self) -> config::RunJsConfig;
    /// Whether calls to `name` may run alongside other calls. The ones that may
    /// not (writers, shell) run one at a time with nothing else in flight, as in
    /// a model's own batch of tool calls.
    fn shares_wave(&self, name: &str) -> bool;
    async fn call(&self, call: ChildCall) -> ChildOutcome;
}

/// Paths a call names, as the program gave them.
pub fn call_paths(name: &str, args: &Value) -> Vec<String> {
    match name {
        "read" | "grep" | "edit" | "write" | "git_diff" => args
            .get("path")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .into_iter()
            .collect(),
        _ => Vec::new(),
    }
}

/// The `run_js` tool.
#[derive(Debug, Clone)]
pub struct RunJs {
    launcher: WorkerLauncher,
}

impl RunJs {
    /// Starts workers in bubblewrap when a call runs; nothing is detected here.
    pub const fn new() -> Self {
        Self {
            launcher: WorkerLauncher::production(),
        }
    }

    pub const fn with_launcher(launcher: WorkerLauncher) -> Self {
        Self { launcher }
    }
}

impl Default for RunJs {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for RunJs {
    fn name(&self) -> &str {
        NAME
    }

    fn description(&self) -> &str {
        description::BASE_DESCRIPTION
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "code": {"type": "string", "minLength": 1, "description": "Body of an async function; top-level await and return work."},
                "timeout_ms": {"type": "integer", "minimum": 1, "maximum": 300_000, "description": "Time limit including the tool calls made inside."}
            },
            "required": ["code"],
            "additionalProperties": false
        })
    }

    /// The tool itself reads and writes nothing: every effect is a child call
    /// that the runtime checks like a normal call.
    fn permissions(&self) -> Permissions {
        Permissions::read_only()
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Exclusive
    }

    async fn execute(&self, _args: Value) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::error(
            "run_js needs a runtime that provides tool access; it cannot run without one",
        ))
    }

    async fn execute_with_context(
        &self,
        ctx: &ToolExecutionContext,
        args: Value,
    ) -> Result<ToolResult, ToolError> {
        let Some(host) = ctx.run_js_host.clone() else {
            return self.execute(args).await;
        };
        let code = args
            .get("code")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs {
                detail: "code must be a string".into(),
            })?
            .to_owned();
        let settings = host.settings();
        let timeout_ms = args
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(settings.default_timeout_ms);
        if timeout_ms == 0 || timeout_ms > settings.max_timeout_ms {
            return Ok(ToolResult::error(format!(
                "timeout_ms must be between 1 and {}",
                settings.max_timeout_ms
            )));
        }
        let call_id = ctx.call_id.clone().unwrap_or_default();
        Ok(self.run(host, &call_id, code, timeout_ms, &settings).await)
    }
}

impl RunJs {
    async fn run(
        &self,
        host: Arc<dyn RunJsHost>,
        call_id: &str,
        code: String,
        timeout_ms: u64,
        settings: &config::RunJsConfig,
    ) -> ToolResult {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let tools = host.tool_names();
        let mut connection = match self.launcher.launch().await {
            Ok(connection) => connection,
            Err(error) => return ToolResult::error(format!("run_js is unavailable: {error}")),
        };
        let start = HostMessage::Run {
            code,
            timeout_ms,
            memory_limit_bytes: u64::from(settings.memory_limit_mib) * 1024 * 1024,
            tools: tools.clone(),
        };
        let mut state = serve::Run::default();
        let ending = serve::serve(
            &mut connection,
            serve::Session {
                host: &host,
                call_id,
                tools: &tools,
                max_tool_calls: settings.max_tool_calls,
                deadline,
            },
            start,
            &mut state,
        )
        .await;
        if matches!(ending, serve::Ending::Failed(_)) {
            connection.settle_stderr().await;
        }
        let stderr = connection.stderr_tail();
        // Kill and reap the worker before reporting anything.
        drop(connection);
        summary::finish(state, ending, &stderr, timeout_ms)
    }
}
