//! The runtime side of `run_js` (ADR 0039): which tools a program may call and
//! how each call runs.
//!
//! A call made inside the program is an ordinary tool call with its own
//! `ToolStarted` / `ToolCompleted` pair (`parent_call_id` = the `run_js` call),
//! validated by the same executor and bound by the same role limits. The one
//! difference is that nothing here may ask the user: a call that would need
//! approval fails and tells the program to make it as a normal tool call.
//!
//! The host also keeps its own record of every call it ran. Budget, escalation
//! and repeat detection read that record, not the `run_js` result's `detail`,
//! which the interface owns and the output limiter may replace.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use providers::ToolSpec;
use sandbox::Action;
use serde_json::Value;
use tools::tools::run_js::{
    CALLABLE_TOOLS, ChildCall, ChildOutcome, RunJsHost, call_paths, description, normalize,
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
        } else if spec.name == "edit" {
            spec.description.push_str(description::EDIT_HINT);
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
    pub(super) fn run_js_host(&self, base: &ToolExecutionContext) -> Option<Arc<RuntimeRunJsHost>> {
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
            records: Mutex::new(HashMap::new()),
            unconfirmed: Arc::clone(&self.run_js_unconfirmed),
        }))
    }

    /// Stops the run when a `run_js` program left something that could not be
    /// confirmed stopped. Returns whether it did.
    pub(super) fn stop_for_run_js_unconfirmed(&mut self) -> bool {
        let detail = self
            .run_js_unconfirmed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let Some(detail) = detail else {
            return false;
        };
        self.finish_error(format!(
            "run_js could not confirm that what it started has stopped ({detail}); no further tools run and the workspace is retained"
        ));
        true
    }

    /// The same condition, for finalization.
    pub(super) fn run_js_unconfirmed_detail(&self) -> Option<String> {
        self.run_js_unconfirmed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// A call the program made, as the host ran it.
pub(crate) struct ChildRecord {
    pub seq: u32,
    pub name: String,
    /// The arguments as the program gave them, before the host adjusted any.
    pub input: Value,
    pub is_error: bool,
    pub paths: Vec<PathBuf>,
}

pub(crate) struct RuntimeRunJsHost {
    executor: Arc<ToolExecutor>,
    base: ToolExecutionContext,
    policy: ExecutionPolicy,
    settings: config::RunJsConfig,
    tools: Vec<String>,
    records: Mutex<HashMap<String, Vec<ChildRecord>>>,
    /// Shared with the loop; see `LoopState::run_js_unconfirmed`.
    unconfirmed: Arc<Mutex<Option<String>>>,
}

impl std::fmt::Debug for RuntimeRunJsHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RuntimeRunJsHost")
            .field("tools", &self.tools)
            .finish_non_exhaustive()
    }
}

impl RuntimeRunJsHost {
    /// The calls made by the `run_js` call `parent`, in program order, removed
    /// from the record.
    pub(crate) fn take_records(&self, parent: &str) -> Vec<ChildRecord> {
        let mut records = self
            .records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(parent)
            .unwrap_or_default();
        records.sort_by_key(|record| record.seq);
        records
    }

    fn record(&self, parent: &str, record: ChildRecord) {
        self.records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(parent.to_owned())
            .or_default()
            .push(record);
    }
}

fn rejected(message: String) -> ChildOutcome {
    ChildOutcome {
        failure: Some(message.clone()),
        result: Err(message),
        is_error: true,
        paths: Vec::new(),
        written: Vec::new(),
        warning: None,
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

/// Closes an unfinished call when its future is dropped (the program ended, the
/// call timed out, or the run was cancelled): the same terminal event a cancelled
/// model call gets, and a record, so the call is counted either way.
struct UnfinishedCall<'a> {
    host: &'a RuntimeRunJsHost,
    ctx: ToolExecutionContext,
    parent: String,
    seq: u32,
    name: String,
    input: Value,
    paths: Vec<String>,
    id: String,
    finished: bool,
}

impl Drop for UnfinishedCall<'_> {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.host
            .executor
            .report_cancelled_call(&self.ctx, &self.name, &self.id, "cancelled");
        self.host.record(
            &self.parent,
            ChildRecord {
                seq: self.seq,
                name: std::mem::take(&mut self.name),
                input: std::mem::take(&mut self.input),
                is_error: true,
                paths: self.paths.iter().map(PathBuf::from).collect(),
            },
        );
    }
}

#[async_trait::async_trait]
impl RunJsHost for RuntimeRunJsHost {
    fn tool_names(&self) -> Vec<String> {
        self.tools.clone()
    }

    fn settings(&self) -> config::RunJsConfig {
        self.settings.clone()
    }

    fn shares_wave(&self, name: &str) -> bool {
        super::tool_calls::shares_wave(&self.executor, name)
    }

    fn report_unconfirmed(&self, detail: &str) {
        self.unconfirmed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_or_insert_with(|| detail.to_owned());
    }

    async fn drain_children(&self, parent_call_id: &str) -> Result<(), String> {
        self.executor
            .drain_child_shell_jobs(&self.base.run_id, parent_call_id)
            .await
            .map_err(|error| error.to_string())
    }

    async fn call(&self, call: ChildCall) -> ChildOutcome {
        let ChildCall {
            parent_call_id,
            seq,
            name,
            args: given,
            remaining,
        } = call;
        let paths = call_paths(&name, &given);
        let refuse = |message: String| {
            self.record(
                &parent_call_id,
                ChildRecord {
                    seq,
                    name: name.clone(),
                    input: given.clone(),
                    is_error: true,
                    paths: paths.iter().map(PathBuf::from).collect(),
                },
            );
            rejected(message)
        };
        if !self.tools.contains(&name) {
            return refuse(format!("tools.{name} is not available in this run"));
        }
        if let Err(error) = self.policy.authorize(&name) {
            return refuse(error.to_string());
        }
        let mut args = given.clone();
        if let Err(message) = check_arguments(&name, &mut args, remaining) {
            return refuse(message);
        }
        let child_id = format!("{parent_call_id}/{seq}");
        let ctx = ToolExecutionContext {
            call_id: Some(child_id.clone()),
            parent_call_id: Some(parent_call_id.clone()),
            ..self.base.clone()
        };
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
                let message = error.agent_message();
                self.record(
                    &parent_call_id,
                    ChildRecord {
                        seq,
                        name: name.clone(),
                        input: given,
                        is_error: true,
                        paths: paths.iter().map(PathBuf::from).collect(),
                    },
                );
                return failed(&message, paths);
            }
        };
        if matches!(validated.action(), Action::Deny | Action::AskFirst) {
            return refuse(format!("tools.{name} {NEEDS_APPROVAL}"));
        }
        // `execute` emits ToolStarted on its first poll, which happens before this
        // future can be dropped, so from here on an unfinished call owes a terminal event.
        let mut guard = UnfinishedCall {
            host: self,
            ctx,
            parent: parent_call_id.clone(),
            seq,
            name: name.clone(),
            input: given.clone(),
            paths: paths.clone(),
            id: child_id,
            finished: false,
        };
        let outcome = validated.scope_approved().execute().await;
        guard.finished = true;
        drop(guard);
        let (is_error, outcome) = match outcome {
            Ok(result) => {
                let normalized = normalize::normalize_full(&name, &args, &result).await;
                let written = if matches!(name.as_str(), "edit" | "write") && !result.is_error {
                    paths.clone()
                } else {
                    Vec::new()
                };
                let warning = (result
                    .detail
                    .as_ref()
                    .and_then(|detail| detail.get("comment_checker"))
                    .and_then(|checker| checker.get("outcome"))
                    .and_then(Value::as_str)
                    == Some("warning"))
                .then(|| {
                    format!(
                        "post-edit checker warning on {}",
                        paths.first().map_or("the edited file", String::as_str)
                    )
                });
                let failure = result
                    .is_error
                    .then(|| first_line(&normalized.text).unwrap_or_else(|| "failed".into()));
                (
                    result.is_error,
                    ChildOutcome {
                        is_error: result.is_error,
                        result: Ok(normalized.into_value()),
                        paths: paths.clone(),
                        written,
                        failure,
                        warning,
                    },
                )
            }
            Err(error) => {
                let message = error.agent_message();
                (true, failed(&message, paths.clone()))
            }
        };
        self.record(
            &parent_call_id,
            ChildRecord {
                seq,
                name,
                input: given,
                is_error,
                paths: paths.iter().map(PathBuf::from).collect(),
            },
        );
        outcome
    }
}

fn first_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}

fn failed(message: &str, paths: Vec<String>) -> ChildOutcome {
    ChildOutcome {
        result: Ok(normalize::Normalized::failure(message).into_value()),
        is_error: true,
        paths,
        written: Vec::new(),
        failure: Some(first_line(message).unwrap_or_else(|| "failed".into())),
        warning: None,
    }
}
