//! `evorch eval report`: metrics, costs repriced from one price table, cached
//! judging and the `report.json` / `report.md` of evaluation directories
//! (ADR 0036 §7–§10).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use catalog::ModelCatalog;
use config::{Config, EvalBudgetConfig};
use event_bus::EventBus;
use routing::EnvLookup;
use sandbox::credential::CredentialStore;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::conversation::Entry;
use crate::cost::{Cost, PriceTable, RequestUsage, Tokens};
use crate::driver::Status;
use crate::judge::{
    self, AspectResult, Judge, Judged, Order, Preferred, ProcessAnalysis, Verdicts,
};
use crate::metrics::{self, RunMetrics, RunRecord};
use crate::output::{self, BindingSummary, Manifest, Outcome};
use crate::roles::{self, SwappedRole};
use crate::runner::{self, BASELINE, CANDIDATE, EvalError, ModelFactory};
use crate::verify::Verification;

const BUS_CAPACITY: usize = 4_096;
const DIFF_BYTES: usize = 60_000;
const ENTRY_CHARS: usize = 2_000;
const CONVERSATION_HEAD: usize = 20;
const CONVERSATION_TAIL: usize = 40;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReportArgs {
    pub dirs: Vec<PathBuf>,
    pub user_config_dir: Option<PathBuf>,
    /// Judge again even where a cached judgment exists.
    pub rejudge: bool,
}

pub struct ReportDeps {
    pub env: Arc<dyn EnvLookup>,
    /// Replaces the configured judge, for tests.
    pub judge: Option<ModelFactory>,
    /// `None` opens `<user config dir>/credentials` when the judge is needed.
    pub credentials: Option<Arc<dyn CredentialStore>>,
    /// The models.dev catalog the price table is built from.
    pub catalog: Option<Arc<ModelCatalog>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PriceSource {
    /// When the models.dev data was fetched, or `None` without a catalog.
    pub catalog_fetched_at: Option<String>,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunSummary {
    pub label: String,
    pub index: u32,
    pub dir: String,
    pub status: Status,
    pub reason: String,
    pub metrics: RunMetrics,
    pub cost: Cost,
    pub verification: Option<Verification>,
    /// Signs that the result depended on resources the replay could not reach.
    pub external_dependencies: Vec<String>,
    pub pushed_refs: usize,
}

impl RunSummary {
    pub fn name(&self) -> String {
        format!("{}-{}", self.label, self.index)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigurationCost {
    pub label: String,
    pub runs: u32,
    pub cost: Cost,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub tokens: Tokens,
    pub cost: Cost,
}

/// Requests the evaluation made itself, kept out of every run's cost.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Overhead {
    pub user_simulation: Usage,
    pub judging: Usage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairReport {
    pub index: u32,
    pub baseline: String,
    pub candidate: String,
    pub verdicts: Vec<AspectResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judging_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<ProcessAnalysis>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AspectTally {
    pub aspect: String,
    pub candidate_wins: u32,
    pub baseline_wins: u32,
    pub ties: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WarningKind {
    SelfPreference,
    CostIncomplete,
    ExternalDependency,
    PrepareFailed,
    JudgingFailed,
    NoPriceCatalog,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Warning {
    pub kind: WarningKind,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub thread_id: String,
    pub problem: Option<String>,
    pub verify: Option<String>,
    pub configurations: Vec<String>,
    pub candidate_overlay: Option<String>,
    pub swapped_roles: Vec<SwappedRole>,
    pub repeat: u32,
    pub budget: EvalBudgetConfig,
    pub simulator: BindingSummary,
    pub judge: BindingSummary,
    pub prices: PriceSource,
    pub runs: Vec<RunSummary>,
    pub costs: Vec<ConfigurationCost>,
    pub overhead: Overhead,
    pub pairs: Vec<PairReport>,
    /// Pairwise winners per aspect over all pairs; never a single ranking.
    pub tally: Vec<AspectTally>,
    pub warnings: Vec<Warning>,
    pub generated_at: String,
}

impl Report {
    pub fn cost_of(&self, label: &str) -> Option<&Cost> {
        self.costs
            .iter()
            .find(|cost| cost.label == label)
            .map(|cost| &cost.cost)
    }
}

/// The judge runs on the user's configuration, never the candidate's, with
/// its binding in the orchestrator slot it is invoked through.
pub fn judge_config(base: &Config) -> (Config, BindingSummary) {
    let mut config = base.clone();
    let summary = match &base.eval.judge {
        Some(binding) => {
            config.agents.orchestrator = binding.clone();
            BindingSummary {
                source: "eval.judge".into(),
                binding: binding.clone(),
            }
        }
        None => BindingSummary {
            source: "agents.orchestrator (eval.judge is unset)".into(),
            binding: base.agents.orchestrator.clone(),
        },
    };
    (config, summary)
}

struct Context<'a> {
    rejudge: bool,
    user_config_dir: Option<PathBuf>,
    table: PriceTable,
    judge_config: Config,
    judge_summary: BindingSummary,
    deps: &'a ReportDeps,
    bus: Arc<EventBus>,
    judge: Option<Result<Arc<dyn runtime::AgentModel>, String>>,
}

impl Context<'_> {
    async fn judge_model(&mut self) -> Result<Arc<dyn runtime::AgentModel>, String> {
        if self.judge.is_none() {
            self.judge = Some(self.compose_judge().await);
        }
        self.judge.clone().unwrap_or_else(|| Err("no judge".into()))
    }

    async fn compose_judge(&self) -> Result<Arc<dyn runtime::AgentModel>, String> {
        if let Some(factory) = &self.deps.judge {
            return Ok(factory(&self.bus));
        }
        let credentials = match &self.deps.credentials {
            Some(store) => Arc::clone(store),
            None => {
                let store = self
                    .user_config_dir
                    .as_ref()
                    .ok_or("cannot resolve a user config directory for the credential store")?
                    .join("credentials");
                tokio::task::spawn_blocking(move || sandbox::credential::open_default(store))
                    .await
                    .map_err(|error| error.to_string())?
                    .map_err(|error| error.to_string())?
            }
        };
        runner::configured_model(
            &self.judge_config,
            self.user_config_dir.as_deref(),
            &credentials,
            &self.deps.env,
            &self.bus,
        )
        .map_err(|error| format!("judge: {error}"))
    }
}

/// Report every directory, writing its `report.json` and `report.md`.
///
/// # Errors
/// Fails when the user configuration cannot be loaded, a directory is not an
/// evaluation directory, or a report cannot be written. A judge that fails is
/// reported in the affected pairs instead.
pub async fn report(args: ReportArgs, deps: &ReportDeps) -> Result<Vec<Report>, EvalError> {
    if args.dirs.is_empty() {
        return Err(EvalError::Usage("at least one --output is required".into()));
    }
    let user_config_dir = args
        .user_config_dir
        .clone()
        .or_else(config::user_config_dir);
    let base = runner::load_config(user_config_dir.as_deref(), None, None)?;
    let (judge_config, judge_summary) = judge_config(&base);
    let mut context = Context {
        rejudge: args.rejudge,
        user_config_dir,
        table: PriceTable::new(base.providers.clone(), deps.catalog.clone()),
        judge_config,
        judge_summary,
        deps,
        bus: Arc::new(EventBus::new(BUS_CAPACITY)),
        judge: None,
    };
    let mut reports = Vec::new();
    for dir in &args.dirs {
        let report = report_dir(dir, &mut context).await?;
        output::write_json(&dir.join(output::REPORT_JSON_FILE), &report)?;
        std::fs::write(dir.join(output::REPORT_MARKDOWN_FILE), markdown(&report))?;
        reports.push(report);
    }
    Ok(reports)
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T, String> {
    let bytes = std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|error| format!("{}: {error}", path.display()))
}

struct LoadedRun {
    summary: RunSummary,
    conversation: Vec<Entry>,
    diff: String,
    record: RunRecord,
}

fn load_run(
    dir: &Path,
    entry: &output::RunEntry,
    table: &PriceTable,
) -> Result<LoadedRun, EvalError> {
    let run_dir = dir.join(&entry.dir);
    let outcome: Outcome =
        read_json(&run_dir.join(output::OUTCOME_FILE)).map_err(EvalError::Usage)?;
    let conversation: Vec<Entry> =
        read_json(&run_dir.join(output::CONVERSATION_FILE)).unwrap_or_default();
    let diff = std::fs::read(run_dir.join(output::DIFF_FILE))
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    let database = run_dir.join(output::DATABASE_FILE);
    let record = RunRecord::load(&database).map_err(|source| EvalError::Database {
        path: database.clone(),
        source,
    })?;
    let summary = RunSummary {
        label: entry.label.clone(),
        index: entry.index,
        dir: entry.dir.clone(),
        status: outcome.status,
        reason: outcome.reason.clone(),
        metrics: RunMetrics::of(&record, &outcome),
        cost: table.cost(record.agent_requests()),
        verification: outcome.verification.clone(),
        external_dependencies: metrics::external_dependencies(&record, &outcome),
        pushed_refs: outcome.pushed_refs.len(),
    };
    Ok(LoadedRun {
        summary,
        conversation,
        diff,
        record,
    })
}

async fn report_dir(dir: &Path, context: &mut Context<'_>) -> Result<Report, EvalError> {
    let manifest: Manifest = read_json(&dir.join(output::MANIFEST_FILE))
        .map_err(|error| EvalError::Usage(format!("not an evaluation directory: {error}")))?;
    let mut runs = Vec::new();
    let mut warnings = Vec::new();
    for entry in &manifest.runs {
        if entry.status.is_none() {
            continue;
        }
        runs.push(load_run(dir, entry, &context.table)?);
    }
    let mut overhead = Overhead::default();
    let simulated: Vec<&RequestUsage> = runs
        .iter()
        .flat_map(|run| run.record.overhead_requests())
        .collect();
    overhead.user_simulation = Usage {
        tokens: Tokens::of(simulated.iter().copied()),
        cost: context.table.cost(simulated.iter().copied()),
    };
    let find = |label: &str, index: u32| {
        runs.iter()
            .find(|run| run.summary.label == label && run.summary.index == index)
    };
    let aspects = judge::aspects(manifest.problem.is_some());
    let mut judging = Vec::new();
    let mut pairs = Vec::new();
    for index in 1..=manifest.repeat {
        let (Some(baseline), Some(candidate)) = (find(BASELINE, index), find(CANDIDATE, index))
        else {
            continue;
        };
        let pair = judge_pair(
            context, dir, &manifest, &aspects, index, baseline, candidate,
        )
        .await;
        judging.extend(pair.usage);
        pairs.push(pair.report);
    }
    overhead.judging = Usage {
        tokens: Tokens::of(&judging),
        cost: context.table.cost(&judging),
    };
    let tally = aspects
        .iter()
        .map(|aspect| {
            let mut tally = AspectTally {
                aspect: (*aspect).to_owned(),
                ..AspectTally::default()
            };
            for result in pairs
                .iter()
                .flat_map(|pair| &pair.verdicts)
                .filter(|result| result.aspect == *aspect)
            {
                match result.winner {
                    Preferred::Candidate => tally.candidate_wins += 1,
                    Preferred::Baseline => tally.baseline_wins += 1,
                    Preferred::Tie => tally.ties += 1,
                }
            }
            tally
        })
        .collect();
    let costs: Vec<ConfigurationCost> = manifest
        .configurations
        .iter()
        .map(|label| {
            let mut cost = Cost::default();
            let mut count = 0;
            for run in runs.iter().filter(|run| &run.summary.label == label) {
                cost.add(&run.summary.cost);
                count += 1;
            }
            ConfigurationCost {
                label: label.clone(),
                runs: count,
                cost,
            }
        })
        .collect();

    if context.table.catalog().is_none() {
        warnings.push(Warning {
            kind: WarningKind::NoPriceCatalog,
            message:
                "the models.dev catalog was unavailable; prices come from the configuration only"
                    .into(),
        });
    }
    if manifest.candidate_overlay.is_some()
        && let Some(message) = self_preference(context, &manifest.swapped_roles)
    {
        warnings.push(Warning {
            kind: WarningKind::SelfPreference,
            message,
        });
    }
    for cost in costs.iter().filter(|cost| !cost.cost.is_complete()) {
        warnings.push(Warning {
            kind: WarningKind::CostIncomplete,
            message: format!(
                "cost incomplete for {}: no price for {}",
                cost.label,
                unpriced_list(&cost.cost)
            ),
        });
    }
    for run in &runs {
        let summary = &run.summary;
        if summary.status == Status::PrepareFailed {
            warnings.push(Warning {
                kind: WarningKind::PrepareFailed,
                message: format!("{} never started: {}", summary.name(), summary.reason),
            });
        }
        if !summary.external_dependencies.is_empty() {
            warnings.push(Warning {
                kind: WarningKind::ExternalDependency,
                message: format!(
                    "{} likely depended on unavailable external resources: {}",
                    summary.name(),
                    summary.external_dependencies.join("; ")
                ),
            });
        }
    }
    for pair in &pairs {
        for error in pair.judging_error.iter().chain(&pair.process_error) {
            warnings.push(Warning {
                kind: WarningKind::JudgingFailed,
                message: format!("pair {}: {error}", pair.index),
            });
        }
    }

    Ok(Report {
        thread_id: manifest.thread_id.clone(),
        problem: manifest.problem.clone(),
        verify: manifest.verify.clone(),
        configurations: manifest.configurations.clone(),
        candidate_overlay: manifest.candidate_overlay.clone(),
        swapped_roles: manifest.swapped_roles.clone(),
        repeat: manifest.repeat,
        budget: manifest.budget.clone(),
        simulator: manifest.simulator.clone(),
        judge: context.judge_summary.clone(),
        prices: PriceSource {
            catalog_fetched_at: context
                .table
                .catalog()
                .map(|catalog| output::timestamp(catalog.fetched_at())),
            note: "models.dev with the configuration's price overrides; recorded costs are ignored"
                .into(),
        },
        runs: runs.into_iter().map(|run| run.summary).collect(),
        costs,
        overhead,
        pairs,
        tally,
        warnings,
        generated_at: output::timestamp(SystemTime::now()),
    })
}

pub(crate) fn unpriced_list(cost: &Cost) -> String {
    cost.unpriced
        .iter()
        .map(|unpriced| {
            format!(
                "{}/{} ({} requests)",
                unpriced.profile.as_deref().unwrap_or(&unpriced.provider),
                unpriced.model,
                unpriced.requests
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// A warning when the judge's model line is one the swapped roles use on either side.
fn self_preference(context: &Context<'_>, swapped: &[SwappedRole]) -> Option<String> {
    let catalog = context.table.catalog();
    let line =
        |model: &roles::ModelRef| roles::family_line(&roles::family(model, catalog)).to_owned();
    let judge_models = roles::role_models(&context.judge_config, "orchestrator");
    let judge_lines: BTreeSet<String> = judge_models.iter().map(line).collect();
    let shared: BTreeSet<String> = swapped
        .iter()
        .flat_map(|role| {
            role.baseline
                .iter()
                .chain(&role.candidate)
                .filter(|model| judge_lines.contains(&line(model)))
                .map(|model| format!("{} uses {} ({})", role.role, model.model, line(model)))
        })
        .collect();
    (!shared.is_empty()).then(|| {
        format!(
            "the judge ({}) shares a model family with the swapped roles: {}",
            judge_models
                .iter()
                .map(|model| model.model.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            shared.into_iter().collect::<Vec<_>>().join("; ")
        )
    })
}

#[derive(Serialize, Deserialize)]
struct Cached<T> {
    baseline: String,
    candidate: String,
    result: T,
    usage: Vec<RequestUsage>,
    judged_at: String,
}

struct JudgedPair {
    report: PairReport,
    usage: Vec<RequestUsage>,
}

async fn judge_pair(
    context: &mut Context<'_>,
    dir: &Path,
    manifest: &Manifest,
    aspects: &[&str],
    index: u32,
    baseline: &LoadedRun,
    candidate: &LoadedRun,
) -> JudgedPair {
    let names = (baseline.summary.name(), candidate.summary.name());
    let judgments = dir.join(output::JUDGMENTS_DIR);
    let mut usage = Vec::new();
    let mut errors = Vec::new();
    let mut orders: BTreeMap<&str, Verdicts> = BTreeMap::new();
    for order in [Order::BaselineFirst, Order::CandidateFirst] {
        let path = judgments.join(format!("pair-{index}-{}.json", order.as_str()));
        let valid =
            |verdicts: &Verdicts| aspects.iter().all(|aspect| verdicts.contains_key(*aspect));
        let (first, second) = match order {
            Order::BaselineFirst => (baseline, candidate),
            Order::CandidateFirst => (candidate, baseline),
        };
        let payload = serde_json::json!({
            "original_user_messages": manifest.original_turns,
            "problem": manifest.problem,
            "A": side(first),
            "B": side(second),
        });
        match cached_or_judge(
            context,
            &manifest.thread_id,
            &path,
            &names,
            valid,
            async |judge: Judge| judge.compare(&payload, aspects).await,
        )
        .await
        {
            Ok((verdicts, spent)) => {
                usage.extend(spent);
                orders.insert(order.as_str(), verdicts);
            }
            Err((error, spent)) => {
                usage.extend(spent);
                errors.push(format!("{} comparison: {error}", order.as_str()));
            }
        }
    }
    let verdicts = match (
        orders.get(Order::BaselineFirst.as_str()),
        orders.get(Order::CandidateFirst.as_str()),
    ) {
        (Some(first), Some(second)) => judge::aggregate(first, second, aspects),
        _ => Vec::new(),
    };
    let swapped: Vec<&str> = manifest
        .swapped_roles
        .iter()
        .map(|role| role.role.as_str())
        .collect();
    let payload = serde_json::json!({
        "problem": manifest.problem,
        "swapped_roles": swapped,
        "baseline": process_side(baseline),
        "candidate": process_side(candidate),
    });
    let process = cached_or_judge(
        context,
        &manifest.thread_id,
        &judgments.join(format!("pair-{index}-process.json")),
        &names,
        |_: &ProcessAnalysis| true,
        async |judge: Judge| judge.analyze(&payload).await,
    )
    .await;
    let (process, process_error) = match process {
        Ok((analysis, spent)) => {
            usage.extend(spent);
            (Some(analysis), None)
        }
        Err((error, spent)) => {
            usage.extend(spent);
            (None, Some(format!("process analysis: {error}")))
        }
    };
    JudgedPair {
        report: PairReport {
            index,
            baseline: names.0,
            candidate: names.1,
            verdicts,
            judging_error: (!errors.is_empty()).then(|| errors.join("; ")),
            process,
            process_error,
        },
        usage,
    }
}

type Spent<T> = Result<(T, Vec<RequestUsage>), (String, Vec<RequestUsage>)>;

/// A cached judgment for the same pair, or a new one cached once it succeeds.
async fn cached_or_judge<T: Serialize + DeserializeOwned>(
    context: &mut Context<'_>,
    thread: &str,
    path: &Path,
    names: &(String, String),
    valid: impl Fn(&T) -> bool,
    ask: impl AsyncFnOnce(Judge) -> Judged<T>,
) -> Spent<T> {
    if !context.rejudge
        && let Ok(cached) = read_json::<Cached<T>>(path)
        && (&cached.baseline, &cached.candidate) == (&names.0, &names.1)
        && valid(&cached.result)
    {
        return Ok((cached.result, cached.usage));
    }
    let model = context
        .judge_model()
        .await
        .map_err(|error| (error, Vec::new()))?;
    let judged = ask(Judge::new(model, Arc::clone(&context.bus), thread)).await;
    let result = match judged.result {
        Ok(result) => result,
        Err(error) => return Err((error.to_string(), judged.usage)),
    };
    let cached = Cached {
        baseline: names.0.clone(),
        candidate: names.1.clone(),
        result,
        usage: judged.usage,
        judged_at: output::timestamp(SystemTime::now()),
    };
    if let Some(parent) = path.parent()
        && let Err(error) =
            std::fs::create_dir_all(parent).and_then(|()| output::write_json(path, &cached))
    {
        tracing::warn!(%error, path = %path.display(), "judgment could not be cached");
    }
    Ok((cached.result, cached.usage))
}

pub(crate) fn clip(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((end, _)) => format!(
            "{}\n[... truncated: {} of {} characters shown ...]",
            &text[..end],
            limit,
            text.chars().count()
        ),
        None => text.to_owned(),
    }
}

fn bounded_diff(diff: &str) -> String {
    if diff.len() <= DIFF_BYTES {
        return diff.to_owned();
    }
    let mut end = DIFF_BYTES;
    while !diff.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n[... diff truncated: {end} of {} bytes shown ...]",
        &diff[..end],
        diff.len()
    )
}

pub(crate) fn bounded_conversation(
    entries: &[Entry],
    head: usize,
    tail: usize,
    chars: usize,
) -> Vec<serde_json::Value> {
    let clipped = |entry: &Entry| {
        let mut entry = entry.clone();
        match &mut entry {
            Entry::User { text } | Entry::Assistant { text } | Entry::Answer { text, .. } => {
                *text = clip(text, chars);
            }
            Entry::Question { title, .. } => *title = clip(title, chars),
        }
        serde_json::to_value(entry).unwrap_or_default()
    };
    if entries.len() <= head + tail {
        return entries.iter().map(clipped).collect();
    }
    let omitted = entries.len() - head - tail;
    entries[..head]
        .iter()
        .map(clipped)
        .chain(std::iter::once(
            serde_json::json!({"kind": "omitted", "entries": omitted}),
        ))
        .chain(entries[entries.len() - tail..].iter().map(clipped))
        .collect()
}

pub(crate) fn compact_metrics(metrics: &RunMetrics) -> serde_json::Value {
    serde_json::json!({
        "input_tokens": metrics.tokens.input(),
        "cache_read_tokens": metrics.tokens.cache_read,
        "output_tokens": metrics.tokens.output,
        "cache_hit_rate_percent": metrics.cache_hit_rate_percent,
        "provider_requests": metrics.tokens.requests,
        "provider_failures": metrics.provider_failures,
        "tool_calls": metrics.tool_calls,
        "tool_errors": metrics.tool_errors,
        "delegations": metrics.delegations.len(),
        "denied_requests": metrics.denied_requests,
        "elapsed_seconds": metrics.elapsed_ms / 1_000,
        "user_turns": metrics.user_turns,
    })
}

pub(crate) fn cost_value(cost: &Cost) -> serde_json::Value {
    serde_json::json!({
        "usd": cost.usd,
        "api_equivalent": cost.is_api_equivalent(),
        "complete": cost.is_complete(),
        "unpriced_models": unpriced_list(cost),
    })
}

fn side(run: &LoadedRun) -> serde_json::Value {
    serde_json::json!({
        "status": run.summary.status,
        "status_reason": run.summary.reason,
        "conversation": bounded_conversation(
            &run.conversation,
            CONVERSATION_HEAD,
            CONVERSATION_TAIL,
            ENTRY_CHARS,
        ),
        "diff": bounded_diff(&run.diff),
        "verification": run.summary.verification,
        "metrics": compact_metrics(&run.summary.metrics),
        "cost": cost_value(&run.summary.cost),
    })
}

fn process_side(run: &LoadedRun) -> serde_json::Value {
    serde_json::json!({
        "status": run.summary.status,
        "status_reason": run.summary.reason,
        "metrics": compact_metrics(&run.summary.metrics),
        "timeline": metrics::timeline(&run.record),
    })
}

pub(crate) fn usd(cost: &Cost) -> String {
    let mut text = format!("${:.4}", cost.usd);
    if cost.is_api_equivalent() {
        text.push_str(" (API-equivalent)");
    }
    if !cost.is_complete() {
        text.push_str(" (cost incomplete)");
    }
    text
}

fn status_name(status: Status) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn winner_name(winner: Preferred) -> &'static str {
    match winner {
        Preferred::Baseline => "baseline",
        Preferred::Candidate => "candidate",
        Preferred::Tie => "tie",
    }
}

fn cell(text: &str) -> String {
    text.replace('|', "\\|").replace('\n', " ")
}

/// The report for people: what ran, what it cost, and what the judge found.
pub fn markdown(report: &Report) -> String {
    let mut out = format!("# Evaluation of thread {}\n\n", report.thread_id);
    if let Some(problem) = &report.problem {
        out.push_str(&format!("**Problem:** {problem}\n\n"));
    }
    if let Some(verify) = &report.verify {
        out.push_str(&format!("**Verification:** `{verify}`\n\n"));
    }
    out.push_str("## Configurations\n\n");
    out.push_str(&format!(
        "- Configurations: {} ({} runs each)\n",
        report.configurations.join(", "),
        report.repeat
    ));
    out.push_str(&format!(
        "- User simulator: {}\n- Judge: {}\n",
        report.simulator.source, report.judge.source
    ));
    for role in &report.swapped_roles {
        let models = |models: &[roles::ModelRef]| {
            models
                .iter()
                .map(|model| model.model.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        out.push_str(&format!(
            "- Swapped `{}`: {} → {}\n",
            role.role,
            models(&role.baseline),
            models(&role.candidate)
        ));
    }
    let budget = &report.budget;
    out.push_str(&format!(
        "- Budget per run: {} user turns, {} tokens, {} tool calls, {} s\n",
        budget.max_user_turns, budget.max_tokens, budget.max_tool_calls, budget.max_elapsed_secs
    ));
    if let Some(overlay) = &report.candidate_overlay {
        out.push_str(&format!(
            "\nCandidate overlay:\n\n```toml\n{}\n```\n",
            overlay.trim_end()
        ));
    }

    out.push_str("\n## Runs\n\n| run | status | input (cached) | output | cache hit | tool calls (errors) | delegations | denied | user turns | time | cost | verification |\n|---|---|---|---|---|---|---|---|---|---|---|---|\n");
    for run in &report.runs {
        let metrics = &run.metrics;
        let verification = run.verification.as_ref().map_or_else(
            || "-".to_owned(),
            |verification| {
                if verification.passed {
                    "passed".to_owned()
                } else if verification.timed_out {
                    "timed out".to_owned()
                } else {
                    format!(
                        "failed ({})",
                        verification
                            .exit_code
                            .map_or_else(|| "no exit".to_owned(), |code| code.to_string())
                    )
                }
            },
        );
        out.push_str(&format!(
            "| {} | {} | {} ({}) | {} | {:.1}% | {} ({}) | {} | {} | {} | {:.1} s | {} | {} |\n",
            run.name(),
            status_name(run.status),
            metrics.tokens.input(),
            metrics.tokens.cache_read,
            metrics.tokens.output,
            metrics.cache_hit_rate_percent,
            metrics.tool_calls.values().sum::<u32>(),
            metrics.tool_errors.values().sum::<u32>(),
            metrics.delegations.len(),
            metrics.denied_requests,
            metrics.user_turns,
            metrics.elapsed_ms as f64 / 1_000.0,
            usd(&run.cost),
            verification
        ));
    }
    out.push_str("\n| run | status reason |\n|---|---|\n");
    for run in &report.runs {
        out.push_str(&format!("| {} | {} |\n", run.name(), cell(&run.reason)));
    }

    out.push_str("\n## Cost\n\n");
    out.push_str(&format!(
        "Prices: {} (catalog fetched {}).\n\n",
        report.prices.note,
        report
            .prices
            .catalog_fetched_at
            .as_deref()
            .unwrap_or("never")
    ));
    for cost in &report.costs {
        out.push_str(&format!(
            "- {} ({} runs): {}\n",
            cost.label,
            cost.runs,
            usd(&cost.cost)
        ));
    }
    out.push_str(&format!(
        "- Evaluation overhead: user simulation {}, judging {}\n",
        usd(&report.overhead.user_simulation.cost),
        usd(&report.overhead.judging.cost)
    ));

    if !report.pairs.is_empty() {
        out.push_str("\n## Pairwise verdicts\n\nCandidate wins / baseline wins / ties per aspect. Each pair is judged in both orders; disagreeing orders count as a tie.\n\n| aspect | candidate | baseline | tie |\n|---|---|---|---|\n");
        for tally in &report.tally {
            out.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                tally.aspect, tally.candidate_wins, tally.baseline_wins, tally.ties
            ));
        }
        for pair in &report.pairs {
            out.push_str(&format!(
                "\n### Pair {}: {} vs {}\n\n",
                pair.index, pair.baseline, pair.candidate
            ));
            if let Some(error) = &pair.judging_error {
                out.push_str(&format!("Judging failed: {error}\n\n"));
            }
            for result in &pair.verdicts {
                out.push_str(&format!(
                    "- **{}**: {} — baseline first: {}; candidate first: {}\n",
                    result.aspect,
                    winner_name(result.winner),
                    result.baseline_first.reason,
                    result.candidate_first.reason
                ));
            }
            if let Some(process) = &pair.process {
                out.push_str(&format!(
                    "\nProcess analysis:\n\n- Time and tokens: {}\n- Detours: {}\n- Delegation: {}\n- Swapped roles: {}\n- Difference: {}\n",
                    process.time_and_tokens,
                    process.detours,
                    process.delegation,
                    process.swapped_roles,
                    process.difference
                ));
            }
            if let Some(error) = &pair.process_error {
                out.push_str(&format!("\nProcess analysis failed: {error}\n"));
            }
        }
    }

    if !report.warnings.is_empty() {
        out.push_str("\n## Warnings\n\n");
        for warning in &report.warnings {
            out.push_str(&format!("- {}\n", warning.message));
        }
    }
    out
}

/// Per-thread win/loss/tie counts per aspect, from the candidate's side, and
/// per-thread cost. Threads are listed side by side, never ranked overall.
pub fn cross_thread_table(reports: &[Report]) -> String {
    let aspects: Vec<&str> = judge::aspects(reports.iter().any(|report| report.problem.is_some()));
    let mut out = String::from("| thread |");
    for aspect in &aspects {
        out.push_str(&format!(" {aspect} W/L/T |"));
    }
    out.push_str(" baseline cost | candidate cost |\n|---|");
    out.push_str(&"---|".repeat(aspects.len() + 2));
    out.push('\n');
    for report in reports {
        out.push_str(&format!("| {} |", report.thread_id));
        for aspect in &aspects {
            match report.tally.iter().find(|tally| tally.aspect == *aspect) {
                Some(tally) if !report.pairs.is_empty() => out.push_str(&format!(
                    " {}/{}/{} |",
                    tally.candidate_wins, tally.baseline_wins, tally.ties
                )),
                _ => out.push_str(" - |"),
            }
        }
        let cost = |label| report.cost_of(label).map_or_else(|| "-".to_owned(), usd);
        out.push_str(&format!(" {} | {} |\n", cost(BASELINE), cost(CANDIDATE)));
    }
    out
}

#[cfg(test)]
mod tests;
