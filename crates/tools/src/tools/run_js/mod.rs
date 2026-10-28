//! The `run_js` tool (ADR 0039): JavaScript that calls the agent's own tools.
//!
//! The program runs in a sandboxed `evorch js-worker` process. Each
//! `tools.<name>(args)` becomes a request over [`protocol`] that this module
//! forwards to the [`RunJsHost`] the runtime attaches to the execution context,
//! so role limits, path boundaries and approvals stay the runtime's job. The
//! model receives only what the program prints or returns, plus a short summary.

pub mod description;
mod diff_summary;
pub mod normalize;
pub mod protocol;
mod serve;
mod summary;
#[cfg(test)]
mod tests;
pub mod typings;
pub mod worker;
pub use summary::delivered_cut_off_shells;

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
    /// Whether this `read` is a page the host itself offered as the `next` of an earlier
    /// result of this program, used for the first time. It is one logical read with the
    /// call that offered it, so it does not count toward the call limit or the budgets.
    /// Only `run_js` decides this, from the `next` values it has seen: a program that
    /// writes `{offset: 2}` out of nowhere does not get it.
    pub continuation: bool,
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
    /// Called once the program's calls were dropped: stops the commands the `parent`
    /// call started and waits until their processes are gone. An error means one may
    /// still be alive, and `run_js` then fails instead of reporting a clean end.
    async fn drain_children(&self, _parent_call_id: &str) -> Result<(), String> {
        Ok(())
    }
    /// Something `run_js` started may still be running and could not be stopped or
    /// confirmed stopped (`detail` says what). The host must stop running tools for
    /// this run: that state is reported by the host itself, not by a result the
    /// model or the output limiter could reshape.
    fn report_unconfirmed(&self, _detail: &str) {}
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
    #[cfg(test)]
    bounds: Option<serve::Bounds>,
    #[cfg(test)]
    expire: Option<Arc<tokio::sync::Notify>>,
}

impl RunJs {
    /// Starts workers in bubblewrap when a call runs; nothing is detected here.
    pub const fn new() -> Self {
        Self::with_launcher(WorkerLauncher::production())
    }

    pub const fn with_launcher(launcher: WorkerLauncher) -> Self {
        Self {
            launcher,
            #[cfg(test)]
            bounds: None,
            #[cfg(test)]
            expire: None,
        }
    }

    #[cfg(test)]
    fn with_expiry(mut self, expire: Arc<tokio::sync::Notify>) -> Self {
        self.expire = Some(expire);
        self
    }

    #[cfg(test)]
    fn with_bounds(mut self, bounds: serve::Bounds) -> Self {
        self.bounds = Some(bounds);
        self
    }

    fn bounds(&self) -> serve::Bounds {
        #[cfg(test)]
        if let Some(bounds) = self.bounds {
            return bounds;
        }
        serve::Bounds::default()
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
        // Preparing the worker (locating it, listing its libraries) counts against the
        // call's deadline too, and is cancelled, its processes killed, at the deadline.
        let unconfirmed: Arc<dyn Fn(String) + Send + Sync> = {
            let host = Arc::clone(&host);
            Arc::new(move |detail| host.report_unconfirmed(&detail))
        };
        let mut connection = match self.launcher.launch(deadline, unconfirmed).await {
            Ok(connection) => connection,
            Err(worker::LaunchError::Failed(error)) => {
                return ToolResult::error(format!("run_js is unavailable: {error}"));
            }
            Err(worker::LaunchError::NotReady) => {
                return ToolResult::error(format!(
                    "run_js is unavailable: the worker was not ready within {timeout_ms} ms"
                ));
            }
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
                bounds: self.bounds(),
                #[cfg(test)]
                expire: self.expire.clone(),
            },
            start,
            &mut state,
        )
        .await;
        state.drain_error = host.drain_children(call_id).await.err();
        if let Some(error) = &state.drain_error {
            host.report_unconfirmed(&format!(
                "commands started by run_js were not confirmed stopped: {error}"
            ));
        }
        if matches!(ending, serve::Ending::Failed(_)) {
            connection.settle_stderr().await;
        }
        let stderr = connection.stderr_tail();
        // Kill and reap the worker before reporting anything.
        drop(connection);
        summary::finish(state, ending, &stderr, timeout_ms, settings)
    }
}
