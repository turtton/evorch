//! The Evaluator role's tools (ADR 0036 §1): list and inspect recorded threads
//! in the user's database, pin them as evaluation samples, and read evaluation
//! reports. Every result is bounded so it fits in a model's context.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use catalog::ModelCatalog;
use serde::Deserialize;
use serde_json::{Value, json};
use storage::eval_sample::EvalSample;
use storage::thread_start::ThreadStartSummary;
use storage::{Database, StorageHandle};
use tools::{Permissions, Tool, ToolError, ToolExecutionMode, ToolResult};

use crate::conversation;
use crate::cost::PriceTable;
use crate::metrics::{self, RunMetrics, RunRecord};
use crate::output::{self, Manifest};
use crate::report::{self, Report};
use crate::roles;
use crate::source::{ChatThread, OriginalThread};

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
const COST_NOTE: &str = "repriced from current prices; requests older than the usage retention are rolled up into daily totals and not included";

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
}

struct Shared {
    context: EvaluatorContext,
    catalog: tokio::sync::OnceCell<Option<Arc<ModelCatalog>>>,
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
    let shared = Arc::new(Shared {
        context,
        catalog: tokio::sync::OnceCell::new(),
    });
    vec![
        Arc::new(ListThreads(Arc::clone(&shared))),
        Arc::new(InspectThread(Arc::clone(&shared))),
        Arc::new(SaveSample(Arc::clone(&shared))),
        Arc::new(RemoveSample(Arc::clone(&shared))),
        Arc::new(ReadReport(shared)),
    ]
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
        "updated_at": timestamp(sample.updated_at_ns),
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
    let runs = thread_runs(database, chat.as_ref(), &summary.root_run_id)?;
    let requests: Vec<_> = database
        .usage_requests_for_runs(&runs)
        .map_err(storage_error)?
        .iter()
        .map(|row| crate::cost::RequestUsage::from_record(&row.record))
        .collect();
    let mut entry = json!({
        "thread_id": summary.thread_id,
        "first_request": first_request,
        "project_root": summary.project_root,
        "captured_at": timestamp(summary.captured_at_ns),
        "replayable": refusal.is_none(),
        "cost": report::cost_value(&prices.cost(requests.iter().filter(|request| !request.is_overhead()))),
        "sample": sample.map(sample_value),
    });
    if let Some(refusal) = refusal {
        entry["not_replayable"] = Value::String(refusal);
    }
    Ok(entry)
}

#[async_trait::async_trait]
impl Tool for ListThreads {
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
    let denied = metrics::denied_executions(&record);
    let user_turns = u32::try_from(chat.turns.len()).unwrap_or(u32::MAX);
    let run_metrics =
        RunMetrics::from_record(&record, denied.len(), record.elapsed_ms(), user_turns);
    let entries = conversation::recorded(&chat.turns, &record.events, &chat.lineage);
    let refusal = replay_refusal(database, thread_id);
    let sample = database.eval_sample(thread_id).map_err(storage_error)?;
    let mut value = json!({
        "thread_id": thread_id,
        "role": chat.role.name(),
        "project_root": start.project_root,
        "captured_at": timestamp(start.captured_at_ns),
        "replayable": refusal.is_none(),
        "sample": sample.as_ref().map(sample_value),
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
        "timeline": metrics::timeline_within(&record, TIMELINE_HEAD, TIMELINE_TAIL),
        "cost": report::cost_value(&prices.cost(record.agent_requests())),
        "cost_note": COST_NOTE,
    });
    if let Some(refusal) = refusal {
        value["not_replayable"] = Value::String(refusal);
    }
    if events_truncated {
        value["events_note"] = Value::String(format!(
            "only the first {MAX_EVENTS} events were read; the timeline and metrics miss later ones"
        ));
    }
    Ok(value)
}

#[async_trait::async_trait]
impl Tool for InspectThread {
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

#[async_trait::async_trait]
impl Tool for SaveSample {
    fn name(&self) -> &'static str {
        "eval_save_sample"
    }

    fn description(&self) -> &'static str {
        "Pin a thread as an evaluation sample with the problem observed in it and the roles the evaluation focuses on; saving again replaces both. Threads that cannot be replayed are refused with the reason. A pinned thread is kept past its retention until the sample is removed."
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
        let thread_id = args.thread_id;
        let checked = {
            let thread_id = thread_id.clone();
            self.0
                .read(move |database| {
                    OriginalThread::load(database, &thread_id).map_err(|error| {
                        format!("thread {thread_id} cannot be pinned as a sample: {error}")
                    })?;
                    Ok(())
                })
                .await
        };
        if let Err(message) = checked {
            return Ok(ToolResult::error(message));
        }
        let now = now_ns();
        let sample = EvalSample {
            thread_id: thread_id.clone(),
            problem,
            focus_roles,
            created_at_ns: now,
            updated_at_ns: now,
        };
        let saved = sample_value(&sample);
        Ok(respond(
            self.0
                .write(move |storage| {
                    storage.save_eval_sample(sample).map_err(storage_error)?;
                    Ok(json!({"pinned": thread_id, "sample": saved}))
                })
                .await,
        ))
    }
}

struct RemoveSample(Arc<Shared>);

#[async_trait::async_trait]
impl Tool for RemoveSample {
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
    json!({
        "runs": manifest.runs.len(),
        "finished_runs": manifest.runs.iter().filter(|run| run.finished_at.is_some()).count(),
        "finished": manifest.finished_at.is_some(),
    })
}

fn list_evaluations(root: &Path) -> Result<Value, String> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(root)
        .map_err(|error| format!("{}: {error}", root.display()))?
        .filter_map(Result::ok)
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
                    "swapped_roles": clip(&process.swapped_roles, ANALYSIS_CHARS),
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
        "repeat": report.repeat,
        "tally": report.tally,
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

fn read_report(user_config_dir: &Path, dir: Option<&str>) -> Result<Value, String> {
    let root = user_config_dir.join("eval");
    let Ok(root) = root.canonicalize() else {
        return Ok(json!({"root": root, "evaluations": [], "more": 0}));
    };
    let Some(dir) = dir else {
        return list_evaluations(&root);
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
        "progress": progress(&manifest),
        "note": format!("no report yet; once the runs finish, `evorch eval report --output {}` writes it", dir.display()),
    }))
}

#[async_trait::async_trait]
impl Tool for ReadReport {
    fn name(&self) -> &'static str {
        "eval_report"
    }

    fn description(&self) -> &'static str {
        "Without dir, list recent evaluations with their progress. With dir, summarize that evaluation's report: per-aspect results with the judge's reasons, run statuses and metrics, costs, warnings and the process analysis. Only directories under the evaluation directory can be read."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "dir": {"type": "string", "minLength": 1, "description": "An evaluation directory name as listed, or its path."},
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
        let user_config_dir = self.0.context.user_config_dir.clone();
        let result =
            tokio::task::spawn_blocking(move || read_report(&user_config_dir, args.dir.as_deref()))
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result);
        Ok(respond(result))
    }
}

#[cfg(test)]
mod tests;
