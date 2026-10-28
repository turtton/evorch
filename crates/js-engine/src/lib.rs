//! QuickJS (via rquickjs) engine behind the `run_js` tool (ADR 0039).
//!
//! The engine runs one JavaScript program per call in a fresh context. It
//! exposes `tools.<name>(args)` (delegated to [`HostCalls`]), `readText(path)`,
//! `console.*` / `print`, and nothing else from the outside world: no `std` /
//! `os` module, no `import`, no timers, no network. The caller is expected to
//! run this inside a separate sandboxed process (`evorch js-worker`).

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rquickjs::prelude::{Async, Func};
use rquickjs::{
    AsyncContext, AsyncRuntime, CatchResultExt, Ctx, Function, Object, Value, context::EvalOptions,
};
use serde::{Deserialize, Serialize};

/// File name that appears in JS stack traces for the user's program.
pub const USER_FILE_NAME: &str = "run_js";
const PRELUDE_FILE_NAME: &str = "<prelude>";
const PRELUDE: &str = include_str!("prelude.js");
/// Stack budget for JS frames. QuickJS raises `RangeError` beyond it.
const MAX_STACK_BYTES: usize = 1024 * 1024;
/// Upper bound for the total text printed by one program.
pub const MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

/// What the JS program may reach outside the engine.
#[async_trait(?Send)]
pub trait HostCalls {
    /// Runs the tool `name`. `Ok` is the JSON value `tools.<name>()` resolves
    /// to; `Err` becomes a thrown `Error` carrying the message.
    async fn call_tool(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String>;

    /// Receives text printed by `console.log` / `print`.
    fn output(&self, text: &str);
}

#[derive(Debug, Clone)]
pub struct RunOptions {
    /// Wall-clock budget, including time spent waiting for host calls.
    pub timeout: Duration,
    pub memory_limit_bytes: usize,
    /// Names exposed as `tools.<name>`.
    pub tools: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JsError {
    pub message: String,
    #[serde(default)]
    pub stack: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunOutcome {
    /// The returned value as JSON; `null` for `undefined` or on error.
    pub value: serde_json::Value,
    pub error: Option<JsError>,
    /// The time budget was exhausted (the error says so too).
    pub timed_out: bool,
}

impl RunOutcome {
    fn failed(message: impl Into<String>) -> Self {
        Self {
            value: serde_json::Value::Null,
            error: Some(JsError {
                message: message.into(),
                stack: String::new(),
            }),
            timed_out: false,
        }
    }
}

/// Runs `code` as the body of an async function (top-level `await` and
/// `return` work). The wrapper is kept on the first line so that line numbers
/// in exceptions equal the user's line numbers.
pub async fn run(code: &str, options: RunOptions, host: Rc<dyn HostCalls>) -> RunOutcome {
    let started = Instant::now();
    let deadline = started + options.timeout;
    let timed_out = Arc::new(AtomicBool::new(false));

    let runtime = match AsyncRuntime::new() {
        Ok(runtime) => runtime,
        Err(err) => return RunOutcome::failed(format!("JS runtime initialisation failed: {err}")),
    };
    runtime.set_memory_limit(options.memory_limit_bytes).await;
    runtime.set_max_stack_size(MAX_STACK_BYTES).await;
    {
        let timed_out = Arc::clone(&timed_out);
        runtime
            .set_interrupt_handler(Some(Box::new(move || {
                if Instant::now() >= deadline {
                    timed_out.store(true, Ordering::SeqCst);
                    true
                } else {
                    false
                }
            })))
            .await;
    }
    let context = match AsyncContext::full(&runtime).await {
        Ok(context) => context,
        Err(err) => return RunOutcome::failed(format!("JS context initialisation failed: {err}")),
    };

    let wrapped = format!("(async function () {{{code}\n}})");
    let tools = options.tools;
    let execution =
        context.async_with(
            async move |ctx| match execute(&ctx, &wrapped, tools, host).await {
                Ok(outcome) => outcome,
                Err(error) => RunOutcome {
                    value: serde_json::Value::Null,
                    error: Some(error),
                    timed_out: false,
                },
            },
        );

    let remaining = deadline.saturating_duration_since(Instant::now());
    let mut outcome = match tokio::time::timeout(remaining, execution).await {
        Ok(outcome) => outcome,
        Err(_) => {
            timed_out.store(true, Ordering::SeqCst);
            RunOutcome::failed("")
        }
    };
    if timed_out.load(Ordering::SeqCst) {
        outcome.timed_out = true;
        outcome.value = serde_json::Value::Null;
        outcome.error = Some(JsError {
            message: format!("run_js timed out after {} ms", options.timeout.as_millis()),
            stack: outcome.error.map(|e| e.stack).unwrap_or_default(),
        });
    }
    outcome
}

async fn execute<'js>(
    ctx: &Ctx<'js>,
    wrapped: &str,
    tools: Vec<String>,
    host: Rc<dyn HostCalls>,
) -> Result<RunOutcome, JsError> {
    let driver = install(ctx, tools, host).map_err(|err| describe(ctx, err))?;
    let mut options = EvalOptions::default();
    options.global = true;
    options.strict = false;
    options.filename = Some(USER_FILE_NAME.to_string());
    let main: Function = ctx
        .eval_with_options(wrapped.as_bytes().to_vec(), options)
        .map_err(|err| describe(ctx, err))?;
    let run: Function = driver.get("run").map_err(|err| describe(ctx, err))?;
    let promise: rquickjs::Promise = run.call((main,)).map_err(|err| describe(ctx, err))?;
    let json: String = promise
        .into_future::<String>()
        .await
        .map_err(|err| describe(ctx, err))?;
    let envelope: serde_json::Value = serde_json::from_str(&json).map_err(|err| JsError {
        message: format!("run_js produced an invalid result envelope: {err}"),
        stack: String::new(),
    })?;
    if envelope.get("ok").and_then(serde_json::Value::as_bool) == Some(true) {
        Ok(RunOutcome {
            value: envelope
                .get("value")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
            error: None,
            timed_out: false,
        })
    } else {
        Ok(RunOutcome {
            value: serde_json::Value::Null,
            error: Some(JsError {
                message: envelope
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown error")
                    .to_string(),
                stack: envelope
                    .get("stack")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            }),
            timed_out: false,
        })
    }
}

fn install<'js>(
    ctx: &Ctx<'js>,
    tools: Vec<String>,
    host: Rc<dyn HostCalls>,
) -> rquickjs::Result<Object<'js>> {
    let globals = ctx.globals();
    globals.set("__toolNames", tools)?;

    let call_host = Rc::clone(&host);
    globals.set(
        "__hostCall",
        Function::new(
            ctx.clone(),
            Async(move |name: String, args: String| {
                let host = Rc::clone(&call_host);
                async move {
                    let reply = match serde_json::from_str::<serde_json::Value>(&args) {
                        Err(err) => serde_json::json!({
                            "ok": false,
                            "error": format!("invalid arguments for tools.{name}: {err}"),
                        }),
                        Ok(args) => match host.call_tool(&name, args).await {
                            Ok(value) => serde_json::json!({"ok": true, "value": value}),
                            Err(error) => serde_json::json!({"ok": false, "error": error}),
                        },
                    };
                    Ok::<String, rquickjs::Error>(reply.to_string())
                }
            }),
        )?,
    )?;

    let printed = Rc::new(Cell::new(0usize));
    let print_host = Rc::clone(&host);
    globals.set(
        "__hostPrint",
        Func::from(move |text: String| -> bool {
            let total = printed.get().saturating_add(text.len());
            if total > MAX_OUTPUT_BYTES {
                return false;
            }
            printed.set(total);
            print_host.output(&text);
            true
        }),
    )?;

    let mut options = EvalOptions::default();
    options.global = true;
    options.filename = Some(PRELUDE_FILE_NAME.to_string());
    ctx.eval_with_options::<Object, _>(PRELUDE, options)
}

fn describe<'js>(ctx: &Ctx<'js>, error: rquickjs::Error) -> JsError {
    if !matches!(error, rquickjs::Error::Exception) {
        return JsError {
            message: error.to_string(),
            stack: String::new(),
        };
    }
    let caught: Value = ctx.catch();
    let mut message = String::new();
    let mut stack = String::new();
    if let Some(object) = caught.as_object() {
        let name = object.get::<_, Option<String>>("name").ok().flatten();
        let text = object.get::<_, Option<String>>("message").ok().flatten();
        if let Some(text) = text {
            message = match name {
                Some(name) if !name.is_empty() => format!("{name}: {text}"),
                _ => text,
            };
        }
        stack = object
            .get::<_, Option<String>>("stack")
            .ok()
            .flatten()
            .unwrap_or_default();
    }
    if message.is_empty() {
        message = match caught.get::<String>().catch(ctx) {
            Ok(text) => text,
            Err(_) => "uncaught exception".to_string(),
        };
    }
    JsError { message, stack }
}
