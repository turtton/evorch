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
pub mod typings;
pub mod worker;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::task::JoinSet;
use tokio::time::Instant;

use crate::error::ToolError;
use crate::executor::ToolExecutionContext;
use crate::result::ToolResult;
use crate::tool::{Permissions, Tool, ToolExecutionMode};
use protocol::{HostMessage, WorkerMessage};
pub use worker::WorkerLauncher;

pub const NAME: &str = "run_js";
/// Tools a program may call at most (before role limits). Delegation, questions,
/// MCP and network tools are out of scope for the first stage.
pub const CALLABLE_TOOLS: [&str; 6] = ["read", "grep", "git_diff", "edit", "write", "shell"];
/// Bytes of printed output the host keeps; the executor then bounds what the
/// model sees.
const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
/// How long after its own deadline a worker may take to report before the host
/// stops waiting and kills it.
const KILL_GRACE: Duration = Duration::from_secs(2);
/// Detail larger than the executor's 16 KiB ceiling would be dropped entirely.
const DETAIL_BUDGET_BYTES: usize = 12 * 1024;

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
}

/// The runtime side of `run_js`: which tools the program may call and how they
/// run. Attached to [`ToolExecutionContext::run_js_host`].
#[async_trait::async_trait]
pub trait RunJsHost: Send + Sync + std::fmt::Debug {
    /// Names the program may call, as offered in the description.
    fn tool_names(&self) -> Vec<String>;
    fn settings(&self) -> config::RunJsConfig;
    async fn call(&self, call: ChildCall) -> ChildOutcome;
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

/// One call as recorded in `detail.run_js.calls`.
struct Recorded {
    seq: u32,
    name: String,
    is_error: bool,
    paths: Vec<String>,
}

#[derive(Default)]
struct Run {
    output: String,
    output_truncated: bool,
    calls: Vec<Recorded>,
    written: BTreeSet<String>,
    limit_hit: Option<&'static str>,
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

enum Ending {
    Done {
        value: Value,
        error: Option<protocol::JsError>,
        timed_out: bool,
    },
    Failed(String),
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
        let started = Instant::now();
        let deadline = started + Duration::from_millis(timeout_ms);
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
        let mut state = Run::default();
        let ending =
            if let Err(error) = protocol::write_message(&mut connection.writer, &start).await {
                Ending::Failed(format!("cannot start the program: {error}"))
            } else {
                self.serve(
                    &mut connection,
                    &host,
                    call_id,
                    &tools,
                    settings,
                    deadline,
                    &mut state,
                )
                .await
            };
        if matches!(ending, Ending::Failed(_)) {
            connection.settle_stderr().await;
        }
        let stderr = connection.stderr_tail();
        // Kill and reap the worker before reporting anything.
        drop(connection);
        finish(state, ending, &stderr, timeout_ms)
    }

    #[allow(clippy::too_many_arguments)]
    async fn serve(
        &self,
        connection: &mut worker::WorkerConnection,
        host: &Arc<dyn RunJsHost>,
        call_id: &str,
        tools: &[String],
        settings: &config::RunJsConfig,
        deadline: Instant,
        state: &mut Run,
    ) -> Ending {
        let Some(mut reader) = connection.take_reader() else {
            return Ending::Failed("the JS worker's output is unavailable".into());
        };
        // Reading whole lines is not cancel-safe inside select!, so a task owns the reader.
        let (incoming, mut messages) = tokio::sync::mpsc::channel(64);
        let _reader_task = AbortOnDrop(tokio::spawn(async move {
            loop {
                let message = protocol::read_message::<WorkerMessage, _>(&mut reader).await;
                let last = !matches!(message, Ok(Some(_)));
                if incoming.send(message).await.is_err() || last {
                    break;
                }
            }
        }));
        let mut children: JoinSet<(u64, u32, String, ChildOutcome)> = JoinSet::new();
        let mut seq = 0u32;
        let hard_deadline = deadline + KILL_GRACE;
        loop {
            tokio::select! {
                message = messages.recv() => {
                    let message = match message.unwrap_or(Ok(None)) {
                        Ok(Some(message)) => message,
                        Ok(None) => return Ending::Failed("the JS worker exited without finishing".into()),
                        Err(error) => return Ending::Failed(format!("the JS worker sent an invalid message: {error}")),
                    };
                    match message {
                        WorkerMessage::Output { text } => state.push_output(&text),
                        WorkerMessage::Done { value, error, timed_out } => {
                            return Ending::Done { value, error, timed_out };
                        }
                        WorkerMessage::ToolCall { id, name, args } => {
                            seq += 1;
                            let refusal = if seq > settings.max_tool_calls {
                                state.limit_hit = Some("tool_calls");
                                Some(format!(
                                    "tool call limit of {} per run_js call exceeded",
                                    settings.max_tool_calls
                                ))
                            } else if !tools.contains(&name) {
                                Some(format!("tools.{name} is not available in this run"))
                            } else {
                                None
                            };
                            if let Some(error) = refusal {
                                state.calls.push(Recorded { seq, name, is_error: true, paths: Vec::new() });
                                let reply = HostMessage::ToolResult { id, result: None, error: Some(error) };
                                if let Err(error) = protocol::write_message(&mut connection.writer, &reply).await {
                                    return Ending::Failed(format!("cannot reach the JS worker: {error}"));
                                }
                                continue;
                            }
                            let host = Arc::clone(host);
                            let call = ChildCall {
                                parent_call_id: call_id.to_owned(),
                                seq,
                                name: name.clone(),
                                args,
                                remaining: deadline.saturating_duration_since(Instant::now()),
                            };
                            children.spawn(async move {
                                let outcome = host.call(call).await;
                                (id, seq, name, outcome)
                            });
                        }
                    }
                }
                Some(joined) = children.join_next(), if !children.is_empty() => {
                    let (id, seq, name, outcome) = match joined {
                        Ok(finished) => finished,
                        Err(error) => return Ending::Failed(format!("a tool call task failed: {error}")),
                    };
                    state.written.extend(outcome.written.iter().cloned());
                    state.calls.push(Recorded { seq, name, is_error: outcome.is_error, paths: outcome.paths });
                    let reply = match outcome.result {
                        Ok(result) => HostMessage::ToolResult { id, result: Some(result), error: None },
                        Err(error) => HostMessage::ToolResult { id, result: None, error: Some(error) },
                    };
                    if let Err(error) = protocol::write_message(&mut connection.writer, &reply).await {
                        return Ending::Failed(format!("cannot reach the JS worker: {error}"));
                    }
                }
                () = tokio::time::sleep_until(hard_deadline) => {
                    return Ending::Done { value: Value::Null, error: None, timed_out: true };
                }
            }
        }
    }
}

impl Run {
    fn push_output(&mut self, text: &str) {
        let room = MAX_OUTPUT_BYTES.saturating_sub(self.output.len());
        if text.len() <= room {
            self.output.push_str(text);
            return;
        }
        self.output_truncated = true;
        let mut end = room;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        self.output.push_str(&text[..end]);
    }
}

fn finish(mut state: Run, ending: Ending, stderr: &str, timeout_ms: u64) -> ToolResult {
    state.calls.sort_by_key(|call| call.seq);
    let mut content = std::mem::take(&mut state.output);
    let output_bytes = content.len();
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    if state.output_truncated {
        content.push_str("[run_js: output was cut at 8 MiB]\n");
    }
    let mut timed_out = false;
    let mut return_bytes = 0;
    let mut failure = None;
    match ending {
        Ending::Done {
            value,
            error,
            timed_out: expired,
        } => {
            timed_out = expired;
            if expired {
                state.limit_hit = Some("timeout");
            }
            if let Some(error) = error {
                let lowered = error.message.to_lowercase();
                if state.limit_hit.is_none() && lowered.contains("out of memory") {
                    state.limit_hit = Some("memory");
                } else if state.limit_hit.is_none() && lowered.contains("output limit") {
                    state.limit_hit = Some("output");
                }
                failure = Some(if error.stack.trim().is_empty() {
                    error.message
                } else {
                    format!("{}\n{}", error.message, error.stack.trim_end())
                });
            } else if timed_out {
                failure = Some(format!("run_js timed out after {timeout_ms} ms"));
            } else if !value.is_null() {
                let json = value.to_string();
                return_bytes = json.len();
                content.push_str(&format!("=> {json}\n"));
            }
        }
        Ending::Failed(reason) => {
            let stderr = if stderr.is_empty() {
                String::new()
            } else {
                format!(" (worker stderr: {stderr})")
            };
            failure = Some(format!("{reason}{stderr}"));
        }
    }
    if let Some(failure) = &failure {
        content.push_str(failure);
        content.push('\n');
    }
    let failed = state.calls.iter().filter(|call| call.is_error).count();
    let wrote = if state.written.is_empty() {
        String::new()
    } else {
        format!(
            "; wrote: {}",
            state.written.iter().cloned().collect::<Vec<_>>().join(", ")
        )
    };
    content.push_str(&format!(
        "[run_js: {} calls, {failed} failed{wrote}]",
        state.calls.len()
    ));
    let detail = json!({"run_js": run_detail(&state, output_bytes, return_bytes, timed_out)});
    let result = if failure.is_some() {
        ToolResult::error(content)
    } else {
        ToolResult::success(content)
    };
    result.with_detail(detail)
}

fn run_detail(state: &Run, output_bytes: usize, return_bytes: usize, timed_out: bool) -> Value {
    let written: Vec<&String> = state.written.iter().collect();
    let calls_value = |keep_paths: bool, limit: usize| -> Vec<Value> {
        state
            .calls
            .iter()
            .take(limit)
            .map(|call| {
                let paths = if keep_paths { call.paths.clone() } else { Vec::new() };
                json!({"seq": call.seq, "name": call.name, "is_error": call.is_error, "paths": paths})
            })
            .collect()
    };
    let build = |calls: Vec<Value>| {
        json!({
            "calls": calls,
            "calls_omitted": state.calls.len().saturating_sub(calls.len()),
            "output_bytes": output_bytes,
            "return_bytes": return_bytes,
            "timed_out": timed_out,
            "limit_hit": state.limit_hit,
            "written_paths": written,
        })
    };
    let full = build(calls_value(true, usize::MAX));
    if full.to_string().len() <= DETAIL_BUDGET_BYTES {
        return full;
    }
    let without_paths = build(calls_value(false, usize::MAX));
    if without_paths.to_string().len() <= DETAIL_BUDGET_BYTES {
        return without_paths;
    }
    let mut keep = state.calls.len();
    loop {
        keep = keep * 3 / 4;
        let trimmed = build(calls_value(false, keep));
        if keep == 0 || trimmed.to_string().len() <= DETAIL_BUDGET_BYTES {
            return trimmed;
        }
    }
}
