//! The Evaluator role's tools (ADR 0036 §1): list and inspect recorded threads
//! in the user's database, pin them as evaluation samples, start evaluations in
//! the background, read their reports and apply an adopted overlay to the
//! user's configuration. Starting and applying wait for the user's approval.
//! Every result is bounded so it fits in a model's context.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use catalog::ModelCatalog;
use event_bus::EventBus;
use sandbox::approval::{ApprovalGate, ApprovalOutcome};
use serde::Deserialize;
use serde_json::{Value, json};
use storage::eval_sample::{Check, CheckScope, EvalSample, SampleKind, SourceRef};
use storage::thread_start::ThreadStartSummary;
use storage::{Database, StorageHandle};
use tools::{Permissions, Tool, ToolError, ToolExecutionContext, ToolExecutionMode, ToolResult};

use crate::checks;
use crate::conversation;
use crate::cost::{Cost, PriceTable, RequestUsage};
use crate::inspection::LaunchMonitor;
use crate::metrics::{self, RunMetrics, RunRecord};
use crate::output::{self, Manifest};
use crate::report::{self, Report};
use crate::roles;
use crate::source::{ChatThread, OriginalThread};

mod apply;
mod groups;
mod lifecycle;
pub use lifecycle::EvaluationService;
mod start;

pub use groups::find_cli;

const LIST_DEFAULT: usize = 20;
const LIST_MAX: usize = 50;
const PREVIEW_CHARS: usize = 200;
const MAX_EVENTS: usize = 20_000;
const TIMELINE_HEAD: usize = 40;
const TIMELINE_TAIL: usize = 40;
const CONVERSATION_HEAD: usize = 6;
const CONVERSATION_TAIL: usize = 10;
const ENTRY_CHARS: usize = 1_000;
const DENIED_MAX: usize = 20;
const DELEGATIONS_MAX: usize = 40;
const REASON_CHARS: usize = 400;
const ANALYSIS_CHARS: usize = 800;
const REPORT_BYTES: u64 = 8 * 1024 * 1024;
const REPORT_DIRS: usize = 20;
const GROUPS_LISTED: usize = 10;
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);
const COST_NOTE: &str = "repriced from current prices; requests older than the usage retention are rolled up into daily totals and not included";

/// What the host process does for the Evaluator's tools.
pub trait EvaluatorHost: Send + Sync {
    /// Authorize the initiating run, not the read-only evaluation sample.
    fn authorize_run(&self, _run_id: &str) -> Result<(), String> {
        Ok(())
    }

    /// Deliver an evaluation's completion notice to the run that started it,
    /// or the conversation it belongs to, returning the run that received it.
    ///
    /// # Errors
    /// Why no run could receive it.
    fn deliver_notice(&self, run_id: &str, notice: &str) -> Result<String, String>;

    /// Recompose the models after the user's configuration file was saved.
    ///
    /// # Errors
    /// Why the saved configuration could not be applied to running sessions.
    fn reload_config(&self) -> Result<(), String>;
}

/// Where the Evaluator's tools read and write.
#[derive(Clone)]
pub struct EvaluatorContext {
    /// The user's database, read through read-only connections.
    pub database: PathBuf,
    /// The writer of that database, for samples.
    pub storage: StorageHandle,
    /// Holds the configuration prices come from and the `eval/` directory.
    pub user_config_dir: PathBuf,
    /// The models.dev cache; `None` prices from the configuration alone.
    pub catalog_dir: Option<PathBuf>,
    /// Carries approval requests to the user and evaluation diagnostics.
    pub bus: Arc<EventBus>,
    pub host: Arc<dyn EvaluatorHost>,
    /// The `evorch` CLI evaluations run; `None` uses [`find_cli`].
    pub cli: Option<PathBuf>,
}

struct Shared {
    context: EvaluatorContext,
    catalog: tokio::sync::OnceCell<Option<Arc<ModelCatalog>>>,
    groups: LaunchMonitor,
    lifecycle: EvaluationService,
}

impl Shared {
    async fn prices(&self) -> Result<PriceTable, String> {
        let catalog = self
            .catalog
            .get_or_init(|| async {
                let dir = self.context.catalog_dir.as_deref()?;
                ModelCatalog::load_or_refresh(dir).await.ok().map(Arc::new)
            })
            .await
            .clone();
        let config = crate::runner::load_config(Some(&self.context.user_config_dir), None, None)
            .map_err(|error| format!("cannot load the configuration for prices: {error}"))?;
        Ok(PriceTable::new(config.providers, catalog))
    }

    async fn read<T: Send + 'static>(
        &self,
        read: impl FnOnce(&Database) -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        let path = self.context.database.clone();
        tokio::task::spawn_blocking(move || {
            let database = Database::open_read_only(&path)
                .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
            read(&database)
        })
        .await
        .map_err(|error| error.to_string())?
    }

    async fn write<T: Send + 'static>(
        &self,
        write: impl FnOnce(&StorageHandle) -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        let storage = self.context.storage.clone();
        tokio::task::spawn_blocking(move || write(&storage))
            .await
            .map_err(|error| error.to_string())?
    }
}

/// The Evaluator's tools, named as in [`agents::EVALUATOR_TOOLS`].
pub fn tools(context: EvaluatorContext) -> Vec<Arc<dyn Tool>> {
    tools_with_monitor(context).0
}

/// The Evaluator's tools and a read-only view of the evaluations they launch,
/// for an [`crate::inspection::EvaluationInspector`] that must tell a process
/// this one started from one it did not.
pub fn tools_with_monitor(context: EvaluatorContext) -> (Vec<Arc<dyn Tool>>, LaunchMonitor) {
    let (tools, monitor, _) = tools_with_service(context);
    (tools, monitor)
}

/// Host-owned variant: stop admission, kill/reap children, and join notices on shutdown.
pub fn tools_with_service(
    context: EvaluatorContext,
) -> (Vec<Arc<dyn Tool>>, LaunchMonitor, EvaluationService) {
    let monitor = LaunchMonitor::default();
    let lifecycle = EvaluationService::default();
    let shared = Arc::new(Shared {
        context,
        catalog: tokio::sync::OnceCell::new(),
        groups: monitor.clone(),
        lifecycle: lifecycle.clone(),
    });
    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(ListThreads(Arc::clone(&shared))),
        Arc::new(InspectThread(Arc::clone(&shared))),
        Arc::new(SaveSample(Arc::clone(&shared))),
        Arc::new(RemoveSample(Arc::clone(&shared))),
        Arc::new(start::StartEvaluation(Arc::clone(&shared))),
        Arc::new(ReadReport(Arc::clone(&shared))),
        Arc::new(apply::ApplyConfig(shared)),
    ];
    (tools, monitor, lifecycle)
}

/// Show `summary` to the user as an approval request and wait for the answer.
/// Only the user resolves it, so the model cannot start or write anything alone.
async fn ask(
    shared: &Shared,
    ctx: &ToolExecutionContext,
    tool: &str,
    summary: String,
) -> Result<(), String> {
    let call_id = ctx.call_id.as_deref().unwrap_or(tool);
    let input = json!({"kind": tool, "summary": summary});
    match ApprovalGate::new(Arc::clone(&shared.context.bus), APPROVAL_TIMEOUT)
        .request_for(sandbox::approval::ApprovalRequest {
            run_id: &ctx.run_id,
            thread_id: ctx.thread_id.as_deref(),
            tool_name: tool,
            call_id,
            input: Some(input),
        })
        .await
    {
        ApprovalOutcome::Approved => Ok(()),
        ApprovalOutcome::Denied | ApprovalOutcome::Cancelled => Err("the user declined".into()),
        ApprovalOutcome::TimedOut => Err(format!(
            "the user did not answer within {} minutes",
            APPROVAL_TIMEOUT.as_secs() / 60
        )),
    }
}

fn parse<T: serde::de::DeserializeOwned>(args: Value) -> Result<T, ToolError> {
    serde_json::from_value(args).map_err(|error| ToolError::InvalidArgs {
        detail: error.to_string(),
    })
}

fn respond(result: Result<Value, String>) -> ToolResult {
    match result {
        Ok(value) => ToolResult::success(value.to_string()),
        Err(message) => ToolResult::error(message),
    }
}

fn storage_error(error: storage::StorageError) -> String {
    error.to_string()
}

fn timestamp(ns: i64) -> String {
    output::timestamp(storage::ns_to_system_time(ns))
}

fn now_ns() -> i64 {
    storage::system_time_to_ns(SystemTime::now()).unwrap_or_default()
}

fn sample_value(sample: &EvalSample) -> Value {
    json!({
        "problem": metrics::clip(&sample.problem, REASON_CHARS),
        "focus_roles": sample.focus_roles,
        "kind": sample.kind.as_str(),
        "tags": sample.tags,
        "source_refs": sample.source_refs,
        "checks": sample.checks.iter().map(checks::describe).collect::<Vec<_>>(),
        "check_version": sample.check_version,
        "content_hash": sample.content_hash(),
        "updated_at": timestamp(sample.updated_at_ns),
    })
}

/// What was recorded of a thread: its start, chat, runs and events.
struct ThreadRecord {
    start: storage::thread_start::ThreadStartState,
    chat: ChatThread,
    runs: Vec<String>,
    record: RunRecord,
    events_truncated: bool,
}

fn thread_record(database: &Database, thread_id: &str) -> Result<ThreadRecord, String> {
    let start = database
        .thread_start_state(thread_id)
        .map_err(storage_error)?
        .ok_or_else(|| format!("thread {thread_id} has no recorded start state"))?;
    let chat = ChatThread::load(database, thread_id, &start.root_run_id)
        .map_err(|error| error.to_string())?;
    let runs = thread_runs(database, Some(&chat), &start.root_run_id)?;
    let (record, events_truncated) =
        RunRecord::of_runs(database, &runs, start.captured_at_ns, MAX_EVENTS)
            .map_err(storage_error)?;
    Ok(ThreadRecord {
        start,
        chat,
        runs,
        record,
        events_truncated,
    })
}

/// The sample's checks graded on the thread as it was recorded.
///
/// The events are read from the start of the runs, not from the capture of the
/// thread's start state: a run announces itself (and its role, which role
/// scoped checks need) before the capture is taken.
fn grade_recorded(
    database: &Database,
    sample: &EvalSample,
    recorded: &ThreadRecord,
) -> Result<checks::Grading, String> {
    let (record, truncated) = RunRecord::of_runs(database, &recorded.runs, i64::MIN, MAX_EVENTS)
        .map_err(storage_error)?;
    Ok(checks::grade(
        sample,
        &checks::Observation {
            events: &record.events,
            truncated,
            status: None,
            evidence_missing: false,
        },
    ))
}

fn grading_value(grading: &checks::Grading) -> Value {
    json!({
        "quality": grading.quality().label(),
        "summary": grading.tally(),
        "grading": grading,
    })
}

fn replay_refusal(database: &Database, thread_id: &str) -> Option<String> {
    OriginalThread::load(database, thread_id)
        .err()
        .map(|error| error.to_string())
}

/// The thread's runs, delegated ones included, or only its root when the chat
/// cannot be read.
fn thread_runs(
    database: &Database,
    chat: Option<&ChatThread>,
    root_run_id: &str,
) -> Result<Vec<String>, String> {
    let roots = chat.map_or_else(|| vec![root_run_id.to_owned()], |chat| chat.lineage.clone());
    database
        .runs_with_descendants(&roots)
        .map_err(storage_error)
}

/// The repriced cost of the thread's agent requests, delegated runs included.
fn thread_cost(
    database: &Database,
    chat: Option<&ChatThread>,
    root_run_id: &str,
    prices: &PriceTable,
) -> Result<Cost, String> {
    let runs = thread_runs(database, chat, root_run_id)?;
    let requests: Vec<_> = database
        .usage_requests_for_runs(&runs)
        .map_err(storage_error)?
        .iter()
        .map(|row| RequestUsage::from_record(&row.record))
        .collect();
    Ok(prices.cost(requests.iter().filter(|request| !request.is_overhead())))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

struct ListThreads(Arc<Shared>);

fn in_project(root: &str, project: &str) -> bool {
    let project = project.trim_end_matches('/');
    root == project
        || root
            .strip_prefix(project)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn list_entry(
    database: &Database,
    summary: &ThreadStartSummary,
    sample: Option<&EvalSample>,
    prices: &PriceTable,
) -> Result<Value, String> {
    let chat = ChatThread::load(database, &summary.thread_id, &summary.root_run_id).ok();
    let first_request = chat
        .as_ref()
        .and_then(|chat| chat.turns.first())
        .map(|turn| metrics::clip(&turn.turn.text, PREVIEW_CHARS));
    let refusal = match summary.not_replayable {
        Some(reason) => Some(format!("start state is {}", reason.as_str())),
        None => replay_refusal(database, &summary.thread_id),
    };
    let cost = thread_cost(database, chat.as_ref(), &summary.root_run_id, prices)?;
    let mut entry = json!({
        "thread_id": summary.thread_id,
        "first_request": first_request,
        "project_root": summary.project_root,
        "captured_at": timestamp(summary.captured_at_ns),
        "replayable": refusal.is_none(),
        "cost": report::cost_value(&cost),
        "sample": sample.map(sample_value),
    });
    if let Some(refusal) = refusal {
        entry["not_replayable"] = Value::String(refusal);
    }
    Ok(entry)
}

#[async_trait::async_trait]
impl Tool for ListThreads {
    fn fixed_host_description(&self) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "eval_list_threads"
    }

    fn description(&self) -> &'static str {
        "List recorded threads, newest first: the first request, project, start time, whether the thread can be replayed for an evaluation (or why not), its repriced cost and its evaluation sample if pinned. Only threads whose start was recorded are listed."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project": {"type": "string", "description": "Only threads whose project root is this directory or below it."},
                "limit": {"type": "integer", "minimum": 1, "maximum": LIST_MAX},
            },
            "additionalProperties": false,
        })
    }

    fn permissions(&self) -> Permissions {
        Permissions::read_only()
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Shared
    }

    async fn execute(&self, args: Value) -> Result<ToolResult, ToolError> {
        let args: ListArgs = parse(args)?;
        let limit = args.limit.unwrap_or(LIST_DEFAULT).clamp(1, LIST_MAX);
        let prices = match self.0.prices().await {
            Ok(prices) => prices,
            Err(message) => return Ok(ToolResult::error(message)),
        };
        Ok(respond(
            self.0
                .read(move |database| {
                    let samples: HashMap<String, EvalSample> = database
                        .eval_samples()
                        .map_err(storage_error)?
                        .into_iter()
                        .map(|sample| (sample.thread_id.clone(), sample))
                        .collect();
                    let matching: Vec<ThreadStartSummary> = database
                        .thread_start_states_since(i64::MIN)
                        .map_err(storage_error)?
                        .into_iter()
                        .filter(|summary| {
                            args.project
                                .as_deref()
                                .is_none_or(|project| in_project(&summary.project_root, project))
                        })
                        .collect();
                    let threads = matching
                        .iter()
                        .take(limit)
                        .map(|summary| {
                            list_entry(database, summary, samples.get(&summary.thread_id), &prices)
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    Ok(json!({
                        "threads": threads,
                        "more": matching.len().saturating_sub(limit),
                        "cost_note": COST_NOTE,
                    }))
                })
                .await,
        ))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ThreadArgs {
    thread_id: String,
}

struct InspectThread(Arc<Shared>);

fn bounded<T: serde::Serialize>(items: &[T], max: usize) -> Value {
    let mut values: Vec<Value> = items
        .iter()
        .take(max)
        .map(|item| serde_json::to_value(item).unwrap_or_default())
        .collect();
    if items.len() > max {
        values.push(json!({"omitted": items.len() - max}));
    }
    Value::Array(values)
}

fn inspect(database: &Database, thread_id: &str, prices: &PriceTable) -> Result<Value, String> {
    let recorded = thread_record(database, thread_id)?;
    let ThreadRecord {
        start,
        chat,
        record,
        events_truncated,
        ..
    } = &recorded;
    let denied = metrics::denied_executions(record);
    let user_turns = u32::try_from(chat.turns.len()).unwrap_or(u32::MAX);
    let run_metrics =
        RunMetrics::from_record(record, denied.len(), record.elapsed_ms(), user_turns);
    let entries = conversation::recorded(&chat.turns, &record.events, &chat.lineage);
    let refusal = replay_refusal(database, thread_id);
    let sample = database.eval_sample(thread_id).map_err(storage_error)?;
    let checks = sample
        .as_ref()
        .map(|sample| -> Result<Value, String> {
            if sample.checks.is_empty() {
                Ok(json!({"quality": checks::Quality::NoMachineJudgment.label(), "note": "the sample declares no checks"}))
            } else {
                Ok(grading_value(&grade_recorded(database, sample, &recorded)?))
            }
        })
        .transpose()?;
    let mut value = json!({
        "thread_id": thread_id,
        "role": chat.role.name(),
        "project_root": start.project_root,
        "captured_at": timestamp(start.captured_at_ns),
        "replayable": refusal.is_none(),
        "sample": sample.as_ref().map(sample_value),
        "sample_checks": checks,
        "conversation": report::bounded_conversation(
            &entries,
            CONVERSATION_HEAD,
            CONVERSATION_TAIL,
            ENTRY_CHARS,
        ),
        "metrics": report::compact_metrics(&run_metrics),
        "models": run_metrics.models,
        "delegations": bounded(&run_metrics.delegations, DELEGATIONS_MAX),
        "denied_executions": bounded(&denied, DENIED_MAX),
        "timeline": metrics::timeline_within(record, TIMELINE_HEAD, TIMELINE_TAIL),
        "cost": report::cost_value(&prices.cost(record.agent_requests())),
        "cost_note": COST_NOTE,
    });
    if let Some(refusal) = refusal {
        value["not_replayable"] = Value::String(refusal);
    }
    if *events_truncated {
        value["events_note"] = Value::String(format!(
            "only the first {MAX_EVENTS} events were read; the timeline and metrics miss later ones"
        ));
    }
    Ok(value)
}

#[async_trait::async_trait]
impl Tool for InspectThread {
    fn fixed_host_description(&self) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "eval_inspect_thread"
    }

    fn description(&self) -> &'static str {
        "Inspect one recorded thread: the conversation the user saw, its delegations, a bounded event timeline, token, tool and failure metrics, tool calls that were denied, elapsed time and its repriced cost, and whether it can be replayed for an evaluation."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"thread_id": {"type": "string", "minLength": 1}},
            "required": ["thread_id"],
            "additionalProperties": false,
        })
    }

    fn permissions(&self) -> Permissions {
        Permissions::read_only()
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Shared
    }

    async fn execute(&self, args: Value) -> Result<ToolResult, ToolError> {
        let args: ThreadArgs = parse(args)?;
        let prices = match self.0.prices().await {
            Ok(prices) => prices,
            Err(message) => return Ok(ToolResult::error(message)),
        };
        Ok(respond(
            self.0
                .read(move |database| inspect(database, &args.thread_id, &prices))
                .await,
        ))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SaveArgs {
    thread_id: String,
    problem: String,
    #[serde(default)]
    focus_roles: Vec<String>,
    #[serde(default)]
    kind: SampleKind,
    #[serde(default)]
    source_refs: Vec<SourceRef>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    checks: Vec<Check>,
}

struct SaveSample(Arc<Shared>);

fn focus_roles(names: &[String]) -> Result<Vec<String>, String> {
    let mut addresses = Vec::new();
    for name in names {
        let address = roles::address(name).ok_or_else(|| {
            format!(
                "unknown role {name:?}; use one of {}",
                roles::ROLES.join(", ")
            )
        })?;
        if !addresses.iter().any(|known| known == address) {
            addresses.push(address.to_owned());
        }
    }
    Ok(addresses)
}

/// The checks with their role names as config addresses, so that they are
/// stored, hashed and compared in one form.
fn canonical_checks(mut checks: Vec<Check>) -> Result<Vec<Check>, String> {
    for check in &mut checks {
        let scope = match check {
            Check::SuccessfulCalls { scope, .. }
            | Check::ToolErrors { scope, .. }
            | Check::Diagnostics { scope, .. } => scope,
        };
        if let CheckScope::Roles(names) = scope {
            *names = focus_roles(names)?;
        }
    }
    Ok(checks)
}

const CHECKS_SCHEMA_NOTE: &str = "Typed conditions on the recorded thread; there is no expression language. Every check names scope ({\"roles\": [..]} or \"tree\") and tool checks name calls (\"direct\" or \"including_children\", the latter also counting calls made inside run_js).";

fn checks_schema() -> Value {
    let scope = json!({
        "description": "The runs the check looks at.",
        "oneOf": [
            {"const": "tree"},
            {
                "type": "object",
                "properties": {"roles": {"type": "array", "items": {"type": "string", "minLength": 1}, "minItems": 1}},
                "required": ["roles"],
                "additionalProperties": false,
            },
        ],
    });
    let calls = json!({"enum": ["direct", "including_children"]});
    json!({
        "type": "array",
        "maxItems": storage::eval_sample::MAX_CHECKS,
        "description": CHECKS_SCHEMA_NOTE,
        "items": {"oneOf": [
            {
                "type": "object",
                "description": "A tool succeeded at least min and/or at most max times.",
                "properties": {
                    "type": {"const": "successful_calls"},
                    "tool": {"type": "string", "minLength": 1},
                    "min": {"type": "integer", "minimum": 1},
                    "max": {"type": "integer", "minimum": 0},
                    "scope": scope,
                    "calls": calls,
                },
                "required": ["type", "tool", "scope", "calls"],
                "additionalProperties": false,
            },
            {
                "type": "object",
                "description": "A tool ended in an error at most max times.",
                "properties": {
                    "type": {"const": "tool_errors"},
                    "tool": {"type": "string", "minLength": 1},
                    "max": {"type": "integer", "minimum": 0},
                    "scope": scope,
                    "calls": calls,
                },
                "required": ["type", "tool", "max", "scope", "calls"],
                "additionalProperties": false,
            },
            {
                "type": "object",
                "description": "A diagnostic code was raised at most max times.",
                "properties": {
                    "type": {"const": "diagnostics"},
                    "code": {"type": "string", "minLength": 1},
                    "max": {"type": "integer", "minimum": 0},
                    "scope": scope,
                },
                "required": ["type", "code", "max", "scope"],
                "additionalProperties": false,
            },
        ]},
    })
}

#[async_trait::async_trait]
impl Tool for SaveSample {
    fn fixed_host_description(&self) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "eval_save_sample"
    }

    fn description(&self) -> &'static str {
        "Pin a thread as an evaluation sample with the problem observed in it and the roles the evaluation focuses on; saving again replaces everything. Optionally attach kind (failure, the default, or regression), source_refs (runs, events or improvement candidates of the thread the problem was seen in), tags and checks. Checks are graded on the thread's recording before anything is saved, at no provider cost: a failure sample is refused unless at least one check fails on the recording, and a regression sample unless every check passes on a complete recording. A sample without checks gets no machine judgment. Threads that cannot be replayed are refused with the reason. A pinned thread is kept past its retention until the sample is removed."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "thread_id": {"type": "string", "minLength": 1},
                "problem": {"type": "string", "minLength": 1, "description": "What went wrong, as observable behaviour the user agreed to."},
                "focus_roles": {
                    "type": "array",
                    "items": {"type": "string", "minLength": 1},
                    "description": "Role names or config addresses, such as Worker or roles.planner.",
                },
                "kind": {"enum": ["failure", "regression"], "description": "failure (default): a real failure the checks must detect. regression: a thread that went well, whose checks must pass."},
                "source_refs": {
                    "type": "array",
                    "description": "Where the problem was seen. Each must exist and belong to this thread.",
                    "items": {"oneOf": [
                        {"type": "object", "properties": {"type": {"const": "run"}, "run_id": {"type": "string", "minLength": 1}}, "required": ["type", "run_id"], "additionalProperties": false},
                        {"type": "object", "properties": {"type": {"const": "event"}, "event_id": {"type": "integer", "minimum": 1}}, "required": ["type", "event_id"], "additionalProperties": false},
                        {"type": "object", "properties": {"type": {"const": "improvement"}, "candidate_id": {"type": "string", "minLength": 1}}, "required": ["type", "candidate_id"], "additionalProperties": false},
                    ]},
                },
                "tags": {"type": "array", "items": {"type": "string", "minLength": 1}, "description": "Free classification; never changes what a check means."},
                "checks": checks_schema(),
            },
            "required": ["thread_id", "problem"],
            "additionalProperties": false,
        })
    }

    fn permissions(&self) -> Permissions {
        Permissions::read_write()
    }

    async fn execute(&self, args: Value) -> Result<ToolResult, ToolError> {
        let args: SaveArgs = parse(args)?;
        let problem = args.problem.trim().to_owned();
        if problem.is_empty() {
            return Ok(ToolResult::error(
                "describe the problem the sample is about",
            ));
        }
        let focus_roles = match focus_roles(&args.focus_roles) {
            Ok(roles) => roles,
            Err(message) => return Ok(ToolResult::error(message)),
        };
        let checks = match canonical_checks(args.checks) {
            Ok(checks) => checks,
            Err(message) => return Ok(ToolResult::error(message)),
        };
        let thread_id = args.thread_id;
        let now = now_ns();
        let sample = EvalSample {
            kind: args.kind,
            source_refs: args.source_refs,
            tags: args.tags,
            checks,
            ..EvalSample::new(thread_id.clone(), problem, focus_roles, now)
        };
        if let Err(error) = sample.validate() {
            return Ok(ToolResult::error(format!("the sample is invalid: {error}")));
        }
        let checked = {
            let thread_id = thread_id.clone();
            let sample = sample.clone();
            self.0
                .read(move |database| check_sample(database, &thread_id, &sample))
                .await
        };
        let grading = match checked {
            Ok(grading) => grading,
            Err(message) => return Ok(ToolResult::error(message)),
        };
        let mut saved = sample_value(&sample);
        saved["machine_judgment"] = Value::Bool(!sample.checks.is_empty());
        let recording = (!sample.checks.is_empty()).then(|| grading_value(&grading));
        Ok(respond(
            self.0
                .write(move |storage| {
                    storage.save_eval_sample(sample).map_err(storage_error)?;
                    Ok(json!({"pinned": thread_id, "sample": saved, "checks_on_recording": recording}))
                })
                .await,
        ))
    }
}

/// Everything that can be checked without a provider before `sample` is
/// saved: the thread can be replayed, the references belong to it, and the
/// checks tell the failure (or the regression baseline) on its recording.
fn check_sample(
    database: &Database,
    thread_id: &str,
    sample: &EvalSample,
) -> Result<checks::Grading, String> {
    OriginalThread::load(database, thread_id)
        .map_err(|error| format!("thread {thread_id} cannot be pinned as a sample: {error}"))?;
    let recorded = thread_record(database, thread_id)?;
    checks::verify_sources(database, &recorded.runs, &sample.source_refs)
        .map_err(|error| format!("source_refs: {error}"))?;
    let grading = grade_recorded(database, sample, &recorded)?;
    checks::accept(sample, &grading).map_err(|reason| {
        format!(
            "the sample was not saved: {reason}. Checks on the recording: {}",
            grading_value(&grading)
        )
    })?;
    Ok(grading)
}

struct RemoveSample(Arc<Shared>);

#[async_trait::async_trait]
impl Tool for RemoveSample {
    fn fixed_host_description(&self) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "eval_remove_sample"
    }

    fn description(&self) -> &'static str {
        "Unpin a thread's evaluation sample. The thread stays recorded until its retention ends."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"thread_id": {"type": "string", "minLength": 1}},
            "required": ["thread_id"],
            "additionalProperties": false,
        })
    }

    fn permissions(&self) -> Permissions {
        Permissions::read_write()
    }

    async fn execute(&self, args: Value) -> Result<ToolResult, ToolError> {
        let ThreadArgs { thread_id } = parse(args)?;
        Ok(respond(
            self.0
                .write(move |storage| {
                    if storage
                        .delete_eval_sample(&thread_id)
                        .map_err(storage_error)?
                    {
                        Ok(json!({"removed": thread_id}))
                    } else {
                        Err(format!("thread {thread_id} has no evaluation sample"))
                    }
                })
                .await,
        ))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReportArgs {
    #[serde(default)]
    dir: Option<String>,
    #[serde(default)]
    group: Option<String>,
}

struct ReadReport(Arc<Shared>);

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, String> {
    let size = std::fs::metadata(path)
        .map_err(|error| format!("{}: {error}", path.display()))?
        .len();
    if size > REPORT_BYTES {
        return Err(format!(
            "{} is {size} bytes, over the {REPORT_BYTES} byte limit",
            path.display()
        ));
    }
    let bytes = std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|error| format!("{}: {error}", path.display()))
}

/// The evaluation directory `dir` names, refusing anything outside `root`.
fn evaluation_dir(root: &Path, dir: &str) -> Result<PathBuf, String> {
    let path = Path::new(dir);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let resolved = joined
        .canonicalize()
        .map_err(|error| format!("{}: {error}", joined.display()))?;
    if resolved == root || !resolved.starts_with(root) {
        return Err(format!(
            "{} is not an evaluation directory under {}",
            resolved.display(),
            root.display()
        ));
    }
    Ok(resolved)
}

fn progress(manifest: &Manifest) -> Value {
    progress_value(
        manifest.repeat as usize * manifest.configurations.len(),
        manifest.runs.len(),
        manifest
            .runs
            .iter()
            .filter(|run| run.finished_at.is_some())
            .count(),
        manifest.finished_at.is_some(),
    )
}

fn progress_value(planned: usize, runs: usize, finished_runs: usize, finished: bool) -> Value {
    json!({
        "planned_runs": planned,
        "runs": runs,
        "finished_runs": finished_runs,
        "finished": finished,
    })
}

fn evaluation_state(
    running: &HashSet<std::ffi::OsString>,
    dir: &Path,
    manifest: &Manifest,
) -> &'static str {
    if manifest.finished_at.is_some() {
        "finished"
    } else if dir.file_name().is_some_and(|name| running.contains(name)) {
        "running"
    } else {
        "interrupted"
    }
}

fn list_evaluations(shared: &Shared, root: &Path) -> Result<Value, String> {
    let running = shared.groups.running_dirs();
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(root)
        .map_err(|error| format!("{}: {error}", root.display()))?
        .filter_map(Result::ok)
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    dirs.reverse();
    let total = dirs.len();
    let evaluations: Vec<Value> = dirs
        .iter()
        .take(REPORT_DIRS)
        .map(|dir| {
            let name = dir
                .file_name()
                .map(|name| name.to_string_lossy().into_owned());
            match read_json::<Manifest>(&dir.join(output::MANIFEST_FILE)) {
                Ok(manifest) => json!({
                    "dir": name,
                    "thread_id": manifest.thread_id,
                    "problem": manifest.problem.as_deref().map(|problem| metrics::clip(problem, PREVIEW_CHARS)),
                    "configurations": manifest.configurations,
                    "started_at": manifest.started_at,
                    "state": evaluation_state(&running, dir, &manifest),
                    "progress": progress(&manifest),
                    "report": dir.join(output::REPORT_JSON_FILE).is_file(),
                }),
                Err(error) => json!({"dir": name, "error": error}),
            }
        })
        .collect();
    Ok(json!({
        "root": root,
        "evaluations": evaluations,
        "more": total.saturating_sub(REPORT_DIRS),
        "groups": groups::list(shared, root, GROUPS_LISTED),
        "note": "interrupted means no process of this GUI session runs it: it stopped with an earlier session, or an `evorch eval run` started elsewhere is still writing it",
    }))
}

fn models(refs: &[roles::ModelRef]) -> Vec<String> {
    refs.iter()
        .map(|model| format!("{}/{}", model.profile, model.model))
        .collect()
}

fn summarize(dir: &Path, report: &Report) -> Value {
    let clip = |text: &str, chars| metrics::clip(text, chars);
    let pairs: Vec<Value> = report
        .pairs
        .iter()
        .map(|pair| {
            let verdicts: Vec<Value> = pair
                .verdicts
                .iter()
                .map(|verdict| {
                    json!({
                        "aspect": verdict.aspect,
                        "winner": verdict.winner,
                        "basis": verdict.basis,
                        "baseline_first_reason": clip(&verdict.baseline_first.reason, REASON_CHARS),
                        "candidate_first_reason": clip(&verdict.candidate_first.reason, REASON_CHARS),
                    })
                })
                .collect();
            json!({
                "baseline": pair.baseline,
                "candidate": pair.candidate,
                "verdicts": verdicts,
                "judging_error": pair.judging_error.as_deref().map(|error| clip(error, REASON_CHARS)),
                "process": pair.process.as_ref().map(|process| json!({
                    "time_and_tokens": clip(&process.time_and_tokens, ANALYSIS_CHARS),
                    "detours": clip(&process.detours, ANALYSIS_CHARS),
                    "delegation": clip(&process.delegation, ANALYSIS_CHARS),
                    "affected_roles": clip(&process.affected_roles, ANALYSIS_CHARS),
                    "difference": clip(&process.difference, ANALYSIS_CHARS),
                })),
                "process_error": pair.process_error.as_deref().map(|error| clip(error, REASON_CHARS)),
            })
        })
        .collect();
    let runs: Vec<Value> = report
        .runs
        .iter()
        .map(|run| {
            json!({
                "run": run.name(),
                "status": run.status,
                "reason": clip(&run.reason, REASON_CHARS),
                "metrics": report::compact_metrics(&run.metrics),
                "cost": report::cost_value(&run.cost),
                "verification": run.verification.as_ref().map(|verification| json!({
                    "passed": verification.passed,
                    "exit_code": verification.exit_code,
                    "timed_out": verification.timed_out,
                })),
                "external_dependencies": bounded(&run.external_dependencies, 3),
            })
        })
        .collect();
    let costs: BTreeMap<&str, Value> = report
        .costs
        .iter()
        .map(|cost| (cost.label.as_str(), report::cost_value(&cost.cost)))
        .collect();
    json!({
        "dir": dir,
        "thread_id": report.thread_id,
        "problem": report.problem,
        "configurations": report.configurations,
        "swapped_roles": report.swapped_roles.iter().map(|role| json!({
            "role": role.role,
            "baseline": models(&role.baseline),
            "candidate": models(&role.candidate),
        })).collect::<Vec<_>>(),
        "changes": report.changes,
        "repeat": report.repeat,
        "tally": report.tally,
        "validity": report.breakdown.validity,
        "cost_by_payer": json!({
            "agent": report.breakdown.cost.agent.iter().map(|agent| (agent.label.clone(), agent.cost.text())).collect::<BTreeMap<_, _>>(),
            "user_simulator": report.breakdown.cost.simulator.text(),
            "judge": report.breakdown.cost.judge.text(),
            "total": report.breakdown.cost.total.text(),
        }),
        "initial_context": report.breakdown.initial_context,
        "first_request": report.breakdown.first_request,
        "cumulative_usage": report.breakdown.cumulative,
        "pairs": pairs,
        "runs": runs,
        "costs": costs,
        "overhead": {
            "user_simulation": report::cost_value(&report.overhead.user_simulation.cost),
            "judging": report::cost_value(&report.overhead.judging.cost),
        },
        "warnings": report.warnings,
        "prices": report.prices.note,
        "generated_at": report.generated_at,
    })
}

fn read_report(shared: &Shared, args: &ReportArgs) -> Result<Value, String> {
    let root = shared.context.user_config_dir.join("eval");
    let Ok(root) = root.canonicalize() else {
        return Ok(json!({"root": root, "evaluations": [], "more": 0, "groups": []}));
    };
    let dir = match (&args.dir, &args.group) {
        (Some(_), Some(_)) => return Err("pass either dir or group".into()),
        (None, Some(group)) => return groups::detail(shared, &root, group),
        (None, None) => return list_evaluations(shared, &root),
        (Some(dir), None) => dir,
    };
    let dir = evaluation_dir(&root, dir)?;
    let report_file = dir.join(output::REPORT_JSON_FILE);
    if report_file.is_file() {
        return Ok(summarize(&dir, &read_json(&report_file)?));
    }
    let manifest: Manifest = read_json(&dir.join(output::MANIFEST_FILE))
        .map_err(|error| format!("not an evaluation directory: {error}"))?;
    Ok(json!({
        "dir": dir,
        "thread_id": manifest.thread_id,
        "report": null,
        "state": evaluation_state(&shared.groups.running_dirs(), &dir, &manifest),
        "progress": progress(&manifest),
        "note": format!("no report yet; once the runs finish, `evorch eval report --output {}` writes it", dir.display()),
    }))
}

#[async_trait::async_trait]
impl Tool for ReadReport {
    fn fixed_host_description(&self) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "eval_report"
    }

    fn description(&self) -> &'static str {
        "Without arguments, list recent evaluations with their state and progress, and the groups eval_start launched. With group, show each of that group's evaluations with its state, progress or one-line result, and a per-thread table. With dir, summarize that evaluation's report: per-aspect results with the judge's reasons, run statuses and metrics, costs, warnings and the process analysis. Only directories under the evaluation directory can be read."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "dir": {"type": "string", "minLength": 1, "description": "An evaluation directory name as listed, or its path."},
                "group": {"type": "string", "minLength": 1, "description": "A group id from eval_start or the listing."},
            },
            "additionalProperties": false,
        })
    }

    fn permissions(&self) -> Permissions {
        Permissions::read_only()
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Shared
    }

    async fn execute(&self, args: Value) -> Result<ToolResult, ToolError> {
        let args: ReportArgs = parse(args)?;
        let shared = Arc::clone(&self.0);
        let result = tokio::task::spawn_blocking(move || read_report(&shared, &args))
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result);
        Ok(respond(result))
    }
}

#[cfg(test)]
mod tests;
