//! The runtime side of `run_js` (ADR 0039): which tools a program may call and
//! how each call runs.
//!
//! A call made inside the program is an ordinary tool call with its own
//! `ToolStarted` / `ToolCompleted` pair (`parent_call_id` = the `run_js` call),
//! validated by the same executor and bound by the same role limits. The one
//! difference is that nothing here may ask the user: a call that would need
//! approval fails and tells the program to make it as a normal tool call.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use providers::ToolSpec;
use sandbox::Action;
use serde_json::Value;
use tools::tools::run_js::{
    CALLABLE_TOOLS, ChildCall, ChildOutcome, RunJsHost, description, normalize,
};
use tools::{ToolExecutionContext, ToolExecutor};

use super::LoopState;
use crate::ExecutionPolicy;

const NEEDS_APPROVAL: &str = "needs approval or is not allowed here, so run_js did not run it. Make this call as a normal tool call instead.";
const SHELL_ONLY_START: &str = "inside run_js, shell only starts a command and waits for it; make job control calls (poll, stdin, stop, yield_ms, interactive) as normal tool calls";

/// Tools the program may call: the first-stage list intersected with what this
/// run was offered, with their schemas, sorted by name.
pub(crate) fn callable_tools(specs: &[ToolSpec]) -> Vec<(String, Value)> {
    let mut callable: Vec<(String, Value)> = specs
        .iter()
        .filter(|spec| CALLABLE_TOOLS.contains(&spec.name.as_str()))
        .map(|spec| (spec.name.clone(), spec.input_schema.clone()))
        .collect();
    callable.sort_by(|left, right| left.0.cmp(&right.0));
    callable
}

/// Shows `run_js` only when it is offered, with a description generated from
/// this run's tools, and points `shell` at it. Called once per run, before the
/// first request, so the description never changes within a run (prompt cache).
pub(crate) fn apply_run_js_specs(
    specs: &mut Vec<ToolSpec>,
    offered: bool,
    settings: &config::RunJsConfig,
) {
    if !offered {
        specs.retain(|spec| spec.name != tools::tools::run_js::NAME);
        return;
    }
    let callable = callable_tools(specs);
    for spec in specs.iter_mut() {
        if spec.name == tools::tools::run_js::NAME {
            spec.description = description::render(&callable, settings);
        } else if spec.name == "shell" {
            spec.description.push_str(description::SHELL_HINT);
        }
    }
}

impl LoopState {
    /// Whether this run may use `run_js`: switched on, not a team run.
    pub(super) fn run_js_offered(&self) -> bool {
        self.task.config.team.is_none()
            && self
                .runtime()
                .is_some_and(|runtime| runtime.run_js_enabled())
    }

    pub(super) fn apply_run_js(&mut self) {
        let settings = self
            .runtime()
            .map(|runtime| runtime.run_js_settings())
            .unwrap_or_default();
        let offered = self.run_js_offered();
        apply_run_js_specs(&mut self.tool_specs, offered, &settings);
    }

    /// The host that runs the calls of `run_js` programs in this run.
    pub(super) fn run_js_host(&self, base: &ToolExecutionContext) -> Option<Arc<dyn RunJsHost>> {
        if !self.run_js_offered() {
            return None;
        }
        let tools = callable_tools(&self.tool_specs)
            .into_iter()
            .map(|(name, _)| name)
            .filter(|name| self.policy.authorize(name).is_ok())
            .collect();
        Some(Arc::new(RuntimeRunJsHost {
            executor: Arc::clone(&self.shared.executor),
            base: ToolExecutionContext {
                shell_wait_policy: tools::executor::ShellWaitPolicy::UntilTerminal,
                shell_input: None,
                run_js_host: None,
                call_id: None,
                parent_call_id: None,
                ..base.clone()
            },
            policy: self.policy.clone(),
            settings: self
                .runtime()
                .map(|runtime| runtime.run_js_settings())
                .unwrap_or_default(),
            tools,
        }))
    }
}

struct RuntimeRunJsHost {
    executor: Arc<ToolExecutor>,
    base: ToolExecutionContext,
    policy: ExecutionPolicy,
    settings: config::RunJsConfig,
    tools: Vec<String>,
}

impl std::fmt::Debug for RuntimeRunJsHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RuntimeRunJsHost")
            .field("tools", &self.tools)
            .finish_non_exhaustive()
    }
}

fn rejected(message: String) -> ChildOutcome {
    ChildOutcome {
        result: Err(message),
        is_error: true,
        paths: Vec::new(),
        written: Vec::new(),
    }
}

/// Paths named by a call, as given.
fn call_paths(name: &str, args: &Value) -> Vec<String> {
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

/// Restrictions that keep a program from asking for anything a person must
/// decide, or from controlling a job it cannot see.
fn check_arguments(name: &str, args: &mut Value, remaining: Duration) -> Result<(), String> {
    let Some(object) = args.as_object_mut() else {
        return Err(format!("tools.{name} takes an object of arguments"));
    };
    if let Some(access) = object.get("sandbox_access")
        && access.as_str() != Some("isolated")
    {
        return Err(format!(
            "tools.{name} with sandbox_access {access} {NEEDS_APPROVAL}"
        ));
    }
    if name == "shell" {
        if object
            .get("action")
            .is_some_and(|action| action.as_str() != Some("start"))
        {
            return Err(SHELL_ONLY_START.to_owned());
        }
        for key in [
            "job_id",
            "yield_ms",
            "interactive",
            "cursor",
            "input",
            "close_stdin",
            "return_on",
        ] {
            if object.contains_key(key) {
                return Err(SHELL_ONLY_START.to_owned());
            }
        }
        // The command must not outlive the run_js call that started it.
        let left = u64::try_from(remaining.as_millis())
            .unwrap_or(u64::MAX)
            .max(1);
        let timeout = object
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .map_or(left, |requested| requested.min(left));
        object.insert("timeout_ms".into(), timeout.into());
    }
    Ok(())
}

#[async_trait::async_trait]
impl RunJsHost for RuntimeRunJsHost {
    fn tool_names(&self) -> Vec<String> {
        self.tools.clone()
    }

    fn settings(&self) -> config::RunJsConfig {
        self.settings.clone()
    }

    async fn call(&self, call: ChildCall) -> ChildOutcome {
        let ChildCall {
            parent_call_id,
            seq,
            name,
            mut args,
            remaining,
        } = call;
        if !self.tools.contains(&name) {
            return rejected(format!("tools.{name} is not available in this run"));
        }
        if let Err(error) = self.policy.authorize(&name) {
            return rejected(error.to_string());
        }
        if let Err(message) = check_arguments(&name, &mut args, remaining) {
            return rejected(message);
        }
        let child_id = format!("{parent_call_id}/{seq}");
        let ctx = ToolExecutionContext {
            call_id: Some(child_id.clone()),
            parent_call_id: Some(parent_call_id),
            ..self.base.clone()
        };
        let paths = call_paths(&name, &args);
        let validated = match self.executor.validate_call(
            ctx.clone(),
            name.clone(),
            child_id.clone(),
            args.clone(),
        ) {
            Ok(validated) => validated,
            Err(error) => {
                self.executor
                    .report_invalid_call(&ctx, &name, &child_id, args, &error);
                return failed(&error.agent_message(), paths);
            }
        };
        if matches!(validated.action(), Action::Deny | Action::AskFirst) {
            return rejected(format!("tools.{name} {NEEDS_APPROVAL}"));
        }
        match validated.scope_approved().execute().await {
            Ok(result) => {
                let written = if matches!(name.as_str(), "edit" | "write") && !result.is_error {
                    paths.clone()
                } else {
                    Vec::new()
                };
                ChildOutcome {
                    is_error: result.is_error,
                    result: Ok(normalize::normalize(&name, &args, &result).into_value()),
                    paths,
                    written,
                }
            }
            Err(error) => failed(&error.agent_message(), paths),
        }
    }
}

fn failed(message: &str, paths: Vec<String>) -> ChildOutcome {
    ChildOutcome {
        result: Ok(normalize::Normalized::failure(message).into_value()),
        is_error: true,
        paths,
        written: Vec::new(),
    }
}

/// A call a `run_js` program made, read back from the parent's result detail.
pub(crate) struct ChildObservation {
    pub name: String,
    pub is_error: bool,
    pub paths: Vec<PathBuf>,
}

/// The calls recorded in `detail.run_js.calls`, so the escalation detector and
/// the budget see them like calls made directly.
pub(crate) fn child_observations(detail: Option<&Value>) -> Vec<ChildObservation> {
    let Some(calls) = detail
        .and_then(|detail| detail.get("run_js"))
        .and_then(|run| run.get("calls"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    calls
        .iter()
        .filter_map(|call| {
            Some(ChildObservation {
                name: call.get("name")?.as_str()?.to_owned(),
                is_error: call.get("is_error")?.as_bool()?,
                paths: call
                    .get("paths")
                    .and_then(Value::as_array)
                    .map(|paths| {
                        paths
                            .iter()
                            .filter_map(Value::as_str)
                            .map(PathBuf::from)
                            .collect()
                    })
                    .unwrap_or_default(),
            })
        })
        .collect()
}
