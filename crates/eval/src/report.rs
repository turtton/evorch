//! `evorch eval report`: metrics, costs repriced from one price table, cached
//! judging and the `report.json` / `report.md` of evaluation directories
//! (ADR 0036 §7–§10).

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use catalog::ModelCatalog;
use config::{Config, EvalBudgetConfig};
use event_bus::EventBus;
use routing::EnvLookup;
use sandbox::credential::CredentialStore;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use storage::eval_sample::EvalSample;

use crate::changes::Changes;
use crate::checks::{self, Grading};
use crate::conversation::Entry;
use crate::cost::{Cost, PriceTable, RequestUsage, Tokens};
use crate::driver::Status;
use crate::judge::{
    self, AspectResult, Judge, Judged, Order, Preferred, ProcessAnalysis, Verdicts,
};
use crate::metrics::{self, RunMetrics, RunRecord};
use crate::output::{self, BaseContextSummary, BindingSummary, Manifest, Outcome};
use crate::roles::{self, SwappedRole};
use crate::runner::{self, BASELINE, CANDIDATE, EvalError, ModelFactory};
use crate::verify::Verification;

mod breakdown;
mod checks_only;
pub use breakdown::{Breakdown, ComparisonValidity, CostLine, Pending, PendingKind};
pub use checks_only::{Basis, ChecksReport, RunChecks, checks_only_pinned};

const BUS_CAPACITY: usize = 4_096;
const DIFF_BYTES: usize = 60_000;
const ENTRY_CHARS: usize = 2_000;
const CONVERSATION_HEAD: usize = 20;
const CONVERSATION_TAIL: usize = 40;
const FOCUS_OUTPUT_CHARS: usize = 3_000;
const OTHER_OUTPUT_CHARS: usize = 800;
const DELEGATED_FOCUS_MAX: usize = 10;
const DELEGATED_OTHER_MAX: usize = 10;

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
    /// The sample's checks graded on this replay's record; `None` when the
    /// evaluation had no checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checks: Option<Grading>,
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
    /// Pairs in which both orders answered tie: the replays are alike here.
    pub ties: u32,
    /// Pairs in which the orders named different winners: no result, which a
    /// tie both orders gave is not.
    #[serde(default)]
    pub order_disagreements: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WarningKind {
    PreflightFailed,
    SelfPreference,
    CostIncomplete,
    ExternalDependency,
    PrepareFailed,
    JudgingFailed,
    NoPriceCatalog,
    /// The comparison cannot be read as a result yet: see its validity.
    ComparisonPending,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Warning {
    pub kind: WarningKind,
    pub message: String,
}

/// The sample whose checks the runs were graded on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChecksSummary {
    pub sample_hash: String,
    pub kind: storage::eval_sample::SampleKind,
    pub check_version: u32,
    pub checks: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub thread_id: String,
    pub problem: Option<String>,
    /// The checks the runs were graded on; `None` means no machine judgment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checks: Option<ChecksSummary>,
    pub verify: Option<String>,
    pub configurations: Vec<String>,
    pub candidate_overlay: Option<String>,
    pub swapped_roles: Vec<SwappedRole>,
    /// Everything the candidate changes, binding differences included.
    #[serde(default)]
    pub changes: Changes,
    pub repeat: u32,
    pub budget: EvalBudgetConfig,
    pub simulator: BindingSummary,
    pub judge: BindingSummary,
    pub prices: PriceSource,
    pub runs: Vec<RunSummary>,
    /// Replays started and not ended (`label-index`): they have no summary, and
    /// their cost is unknown. Replays never started are not here.
    #[serde(default)]
    pub unfinished: Vec<String>,
    pub costs: Vec<ConfigurationCost>,
    pub overhead: Overhead,
    pub pairs: Vec<PairReport>,
    /// Pairwise winners per aspect over all pairs; never a single ranking.
    pub tally: Vec<AspectTally>,
    pub warnings: Vec<Warning>,
    /// The comparison split by measure, with its validity.
    #[serde(default)]
    pub breakdown: Breakdown,
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
    report_with(args, deps, |_| {}).await
}

/// Like [report], telling `on_written` the index in `args.dirs` of each
/// directory whose report is complete on disk, as soon as it is, so a caller
/// that stops partway knows which ones were written.
///
/// # Errors
/// As [report].
pub async fn report_with(
    args: ReportArgs,
    deps: &ReportDeps,
    on_written: impl FnMut(usize),
) -> Result<Vec<Report>, EvalError> {
    let dirs = args
        .dirs
        .iter()
        .map(|dir| output::PinnedDir::open_path(dir))
        .collect::<Result<Vec<_>, _>>()?;
    report_pinned(dirs, args.user_config_dir, args.rejudge, deps, on_written).await
}

/// Like [report_with] over directories that are already open: every file of the
/// report is read and written through them, so a directory that is swapped for
/// a link after it was chosen, or a link inside it, leads nowhere else.
///
/// # Errors
/// As [report].
pub async fn report_pinned(
    dirs: Vec<output::PinnedDir>,
    user_config_dir: Option<PathBuf>,
    rejudge: bool,
    deps: &ReportDeps,
    mut on_written: impl FnMut(usize),
) -> Result<Vec<Report>, EvalError> {
    if dirs.is_empty() {
        return Err(EvalError::Usage("at least one --output is required".into()));
    }
    let user_config_dir = user_config_dir.or_else(config::user_config_dir);
    let base = runner::load_config(user_config_dir.as_deref(), None, None)?;
    let (judge_config, judge_summary) = judge_config(&base);
    let mut context = Context {
        rejudge,
        user_config_dir,
        table: PriceTable::new(base.providers.clone(), deps.catalog.clone()),
        judge_config,
        judge_summary,
        deps,
        bus: Arc::new(EventBus::new(BUS_CAPACITY)),
        judge: None,
    };
    let mut reports = Vec::new();
    for (index, dir) in dirs.iter().enumerate() {
        let report = report_dir(dir, &mut context).await?;
        dir.write_json(output::REPORT_JSON_FILE, &report)?;
        dir.write(output::REPORT_MARKDOWN_FILE, markdown(&report).as_bytes())?;
        on_written(index);
        reports.push(report);
    }
    Ok(reports)
}

fn read_json_in<T: DeserializeOwned>(dir: &output::PinnedDir, name: &str) -> Result<T, String> {
    let path = dir.join(name);
    let bytes = dir
        .read(name)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|error| format!("{}: {error}", path.display()))
}

/// Read the JSON file at `path`: its directory is opened first and the file is
/// then read without following a link, as [`read_json_in`] does.
#[cfg(test)]
fn read_json<T: DeserializeOwned>(path: &std::path::Path) -> Result<T, String> {
    let invalid = || format!("{}: not a file path", path.display());
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(invalid)?;
    let parent = path.parent().ok_or_else(invalid)?;
    let dir =
        output::PinnedDir::open_root(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    read_json_in(&dir, name)
}

struct LoadedRun {
    summary: RunSummary,
    conversation: Vec<Entry>,
    diff: String,
    record: RunRecord,
    /// The replay's initial context, as its runtime recorded it.
    base_context: Option<BaseContextSummary>,
    /// The replay left no database: its empty record stands for evidence that
    /// could not be read.
    evidence_missing: bool,
}

fn load_run(
    dir: &output::PinnedDir,
    entry: &output::RunEntry,
    table: &PriceTable,
    sample: Option<&EvalSample>,
) -> Result<LoadedRun, EvalError> {
    let run_dir = dir.open_below(&entry.dir)?;
    let outcome: Outcome =
        read_json_in(&run_dir, output::OUTCOME_FILE).map_err(EvalError::Usage)?;
    let conversation: Vec<Entry> =
        read_json_in(&run_dir, output::CONVERSATION_FILE).unwrap_or_default();
    let diff = run_dir
        .read(output::DIFF_FILE)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    let (record, evidence_missing) = load_record(&run_dir)?;
    let summary = RunSummary {
        label: entry.label.clone(),
        index: entry.index,
        dir: entry.dir.clone(),
        status: outcome.status,
        reason: outcome.reason.clone(),
        metrics: RunMetrics::of(&record, &outcome),
        cost: {
            let mut cost = table.cost(record.agent_requests());
            if evidence_missing {
                cost.unrecorded
                    .push(format!("{}-{}", entry.label, entry.index));
            }
            cost
        },
        verification: outcome.verification.clone(),
        external_dependencies: metrics::external_dependencies(&record, &outcome),
        pushed_refs: outcome.pushed_refs.len(),
        checks: sample
            .filter(|sample| !sample.checks.is_empty())
            .map(|sample| grade_run(sample, &record, outcome.status, evidence_missing)),
    };
    let base_context = run_dir
        .read(output::BASE_CONTEXT_FILE)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    Ok(LoadedRun {
        summary,
        conversation,
        diff,
        record,
        base_context,
        evidence_missing,
    })
}

/// The record of the run's database, read from the descriptor of the regular
/// file that was checked, never from its name: the database (and a `-wal` or
/// `-journal` holding unapplied changes) is copied to a private directory
/// from no-follow descriptors, and that copy is opened. A link in its place
/// gives an empty record, and a directory replaced after it was pinned is not
/// followed.
#[cfg(test)]
fn load_database(run_dir: &output::PinnedDir) -> Result<RunRecord, EvalError> {
    load_record(run_dir).map(|(record, _)| record)
}

/// [`load_database`], and whether the database was not there: the empty record
/// then stands for evidence that could not be read, which grading must not
/// take for a run in which nothing happened.
fn load_record(run_dir: &output::PinnedDir) -> Result<(RunRecord, bool), EvalError> {
    let database = run_dir.join(output::DATABASE_FILE);
    let fail = |source| EvalError::Database {
        path: database.clone(),
        source,
    };
    let Some(snapshot) = DatabaseSnapshot::copy(run_dir)? else {
        return Ok((RunRecord::default(), true));
    };
    RunRecord::load(&snapshot.database())
        .map(|record| (record, false))
        .map_err(fail)
}

/// A private copy of a run's database, removed when dropped.
struct DatabaseSnapshot {
    dir: PathBuf,
}

impl DatabaseSnapshot {
    const SIDECARS: [&str; 2] = ["-wal", "-journal"];

    fn copy(run_dir: &output::PinnedDir) -> std::io::Result<Option<Self>> {
        use std::os::unix::fs::DirBuilderExt as _;
        let Some(mut source) = run_dir.open_regular(output::DATABASE_FILE)? else {
            return Ok(None);
        };
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).map_err(|error| std::io::Error::other(error.to_string()))?;
        let name: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let dir = std::env::temp_dir().join(format!("evorch-report-{name}"));
        std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
        let snapshot = Self { dir };
        let mut target = std::fs::File::create_new(snapshot.database())?;
        std::io::copy(&mut source, &mut target)?;
        for suffix in Self::SIDECARS {
            let sidecar = format!("{}{suffix}", output::DATABASE_FILE);
            if let Some(mut source) = run_dir.open_regular(&sidecar)? {
                let mut target = std::fs::File::create_new(snapshot.dir.join(&sidecar))?;
                std::io::copy(&mut source, &mut target)?;
            }
        }
        Ok(Some(snapshot))
    }

    fn database(&self) -> PathBuf {
        self.dir.join(output::DATABASE_FILE)
    }
}

impl Drop for DatabaseSnapshot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn report_dir(
    dir: &output::PinnedDir,
    context: &mut Context<'_>,
) -> Result<Report, EvalError> {
    let manifest: Manifest = read_json_in(dir, output::MANIFEST_FILE)
        .map_err(|error| EvalError::Usage(format!("not an evaluation directory: {error}")))?;
    let mut runs = Vec::new();
    let mut warnings = Vec::new();
    let mut preflight_failure = None;
    if dir.is_regular_file(runner::preflight::REPORT_FILE)?
        && let Some(reason) =
            read_json_in::<runner::preflight::Report>(dir, runner::preflight::REPORT_FILE)
                .map_err(EvalError::Usage)?
                .failure()
    {
        warnings.push(Warning {
            kind: WarningKind::PreflightFailed,
            message: format!(
                "evaluation preflight failed; no replays started and no samples compared: {reason}; evidence: {}",
                runner::preflight::REPORT_FILE,
            ),
        });
        preflight_failure = Some(reason);
    }
    let unfinished: Vec<&output::RunEntry> = manifest
        .runs
        .iter()
        .filter(|entry| entry.status.is_none())
        .collect();
    for entry in &manifest.runs {
        if entry.status.is_none() {
            continue;
        }
        runs.push(load_run(
            dir,
            entry,
            &context.table,
            manifest.sample.as_ref().map(|pinned| &pinned.sample),
        )?);
    }
    let mut overhead = Overhead::default();
    let simulated: Vec<&RequestUsage> = runs
        .iter()
        .flat_map(|run| run.record.overhead_requests())
        .collect();
    overhead.user_simulation = Usage {
        tokens: Tokens::of(simulated.iter().copied()),
        cost: {
            let mut cost = context.table.cost(simulated.iter().copied());
            cost.unfinished.extend(
                unfinished
                    .iter()
                    .map(|entry| format!("{}-{}", entry.label, entry.index)),
            );
            cost.unrecorded.extend(
                runs.iter()
                    .filter(|run| run.evidence_missing)
                    .map(|run| run.summary.name()),
            );
            cost
        },
    };
    let find = |label: &str, index: u32| {
        runs.iter()
            .find(|run| run.summary.label == label && run.summary.index == index)
    };
    let aspects = judge::aspects(manifest.problem.is_some());
    let changes = manifest_changes(&manifest);
    let mut judging = Vec::new();
    let mut pairs = Vec::new();
    for index in 1..=manifest.repeat {
        let (Some(baseline), Some(candidate)) = (find(BASELINE, index), find(CANDIDATE, index))
        else {
            continue;
        };
        let effect = breakdown::effect_of(
            &changes,
            manifest
                .runs
                .iter()
                .find(|entry| entry.label == CANDIDATE && entry.index == index),
        );
        let pair = judge_pair(
            context,
            dir,
            &Pair {
                manifest: &manifest,
                changes: &changes,
                aspects: &aspects,
                index,
                effect: &effect,
            },
            baseline,
            candidate,
        )
        .await;
        judging.extend(pair.usage);
        pairs.push(pair.report);
    }
    overhead.judging = Usage {
        tokens: Tokens::of(&judging),
        cost: context.table.cost(&judging),
    };
    let tally: Vec<AspectTally> = aspects
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
                match (result.winner, result.basis) {
                    (Preferred::Candidate, _) => tally.candidate_wins += 1,
                    (Preferred::Baseline, _) => tally.baseline_wins += 1,
                    (Preferred::Tie, judge::Basis::OrderDisagreement) => {
                        tally.order_disagreements += 1;
                    }
                    (Preferred::Tie, _) => tally.ties += 1,
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
            cost.unfinished.extend(
                unfinished
                    .iter()
                    .filter(|entry| &entry.label == label)
                    .map(|entry| format!("{}-{}", entry.label, entry.index)),
            );
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
        && let Some(message) = self_preference(context, &manifest.swapped_roles, &changes)
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
                "cost incomplete for {}: {}",
                cost.label,
                incomplete_reason(&cost.cost)
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

    let breakdown = breakdown::build(&breakdown::Inputs {
        changes: &changes,
        configurations: &manifest.configurations,
        repeat: manifest.repeat,
        preflight_failure: preflight_failure.as_deref(),
        runs: &runs,
        entries: &manifest.runs,
        costs: &costs,
        overhead: &overhead,
        pairs: &pairs,
        tally: &tally,
    });
    if !breakdown.validity.valid {
        warnings.push(Warning {
            kind: WarningKind::ComparisonPending,
            message: format!(
                "judgment pending: {} reason(s) hold the comparison back from being read as a result (see Validity)",
                breakdown.validity.pending.len()
            ),
        });
    }

    Ok(Report {
        thread_id: manifest.thread_id.clone(),
        problem: manifest.problem.clone(),
        checks: manifest
            .sample
            .as_ref()
            .filter(|pinned| !pinned.sample.checks.is_empty())
            .map(|pinned| ChecksSummary {
                sample_hash: pinned.hash.clone(),
                kind: pinned.sample.kind,
                check_version: pinned.sample.check_version,
                checks: pinned.sample.checks.iter().map(checks::describe).collect(),
            }),
        verify: manifest.verify.clone(),
        configurations: manifest.configurations.clone(),
        candidate_overlay: manifest.candidate_overlay.clone(),
        swapped_roles: manifest.swapped_roles.clone(),
        changes,
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
        unfinished: unfinished
            .iter()
            .map(|entry| format!("{}-{}", entry.label, entry.index))
            .collect(),
        runs: runs.into_iter().map(|run| run.summary).collect(),
        costs,
        overhead,
        pairs,
        tally,
        warnings,
        breakdown,
        generated_at: output::timestamp(SystemTime::now()),
    })
}

/// What the manifest says the candidate changes, with its role binding
/// differences folded in, so an evaluation whose manifest recorded only
/// `swapped_roles` is still described by the roles it swapped.
fn manifest_changes(manifest: &Manifest) -> Changes {
    let mut changes = manifest.changes.clone();
    for role in &manifest.swapped_roles {
        for list in [&mut changes.bindings, &mut changes.affected_roles] {
            if !list.contains(&role.role) {
                list.push(role.role.clone());
                list.sort();
            }
        }
    }
    changes
}

/// The grading of a replay's record against the sample's checks, which also
/// asks whether the replay ended normally.
pub(crate) fn grade_run(
    sample: &EvalSample,
    record: &RunRecord,
    status: Status,
    evidence_missing: bool,
) -> Grading {
    checks::grade(
        sample,
        &checks::Observation {
            events: &record.events,
            truncated: false,
            status: Some(status),
            evidence_missing,
        },
    )
}

/// The "Machine checks" section. An evaluation without checks says so, so
/// that it is not read as a machine-verified one.
fn checks_section(report: &Report) -> String {
    let Some(summary) = &report.checks else {
        return "\n## Machine checks\n\nNo checks are declared for this thread: **no machine judgment**. Only the verification command and the judge speak to quality.\n".to_owned();
    };
    let mut out = format!(
        "\n## Machine checks\n\nSample {} ({}, grading version {}). A check result and the validity of the replay are reported separately: checks that pass on an unfinished replay are not a quality pass.\n\n",
        summary.sample_hash.chars().take(12).collect::<String>(),
        summary.kind.as_str(),
        summary.check_version
    );
    for (index, check) in summary.checks.iter().enumerate() {
        out.push_str(&format!("- check {index}: {check}\n"));
    }
    out.push_str("\n| run | quality | checks | replay record |\n|---|---|---|---|\n");
    for run in &report.runs {
        match &run.checks {
            Some(grading) => out.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                run.name(),
                grading.quality().label(),
                grading.tally(),
                if grading.validity.valid {
                    "complete".to_owned()
                } else {
                    cell(&grading.validity.issues.join("; "))
                }
            )),
            None => out.push_str(&format!("| {} | not graded | - | - |\n", run.name())),
        }
    }
    for run in &report.runs {
        for result in run
            .checks
            .iter()
            .flat_map(|grading| &grading.results)
            .filter(|result| result.verdict != checks::Verdict::Pass)
        {
            out.push_str(&format!(
                "\n- {} check {} {:?}: {}",
                run.name(),
                result.index,
                result.verdict,
                cell(&result.reason)
            ));
        }
    }
    out.push('\n');
    out
}

/// Why a cost is incomplete: models without a price and models whose requests reported no usage.
pub(crate) fn incomplete_reason(cost: &Cost) -> String {
    let mut reasons = Vec::new();
    if !cost.unpriced.is_empty() {
        reasons.push(format!("no price for {}", unpriced_list(cost)));
    }
    if !cost.unreported.is_empty() {
        reasons.push(format!("no reported usage for {}", unreported_list(cost)));
    }
    if !cost.unfinished.is_empty() {
        reasons.push(format!(
            "no recorded end, so an unknown cost, for {}",
            cost.unfinished.join(", ")
        ));
    }
    if !cost.unrecorded.is_empty() {
        reasons.push(format!(
            "no usage record for {}",
            cost.unrecorded.join(", ")
        ));
    }
    reasons.join("; ")
}

pub(crate) fn unpriced_list(cost: &Cost) -> String {
    model_list(&cost.unpriced)
}

pub(crate) fn unreported_list(cost: &Cost) -> String {
    model_list(&cost.unreported)
}

fn model_list(models: &[crate::cost::Unpriced]) -> String {
    models
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

/// A warning when the judge's model line is one the changes involve: the models
/// the swapped roles use on either side, and the models whose tool descriptions
/// the candidate changes.
fn self_preference(
    context: &Context<'_>,
    swapped: &[SwappedRole],
    changes: &Changes,
) -> Option<String> {
    let catalog = context.table.catalog();
    let line =
        |model: &roles::ModelRef| roles::family_line(&roles::family(model, catalog)).to_owned();
    let judge_models = roles::role_models(&context.judge_config, "orchestrator");
    let judge_lines: BTreeSet<String> = judge_models.iter().map(line).collect();
    let mut shared: BTreeSet<String> = swapped
        .iter()
        .flat_map(|role| {
            role.baseline
                .iter()
                .chain(&role.candidate)
                .filter(|model| judge_lines.contains(&line(model)))
                .map(|model| format!("{} uses {} ({})", role.role, model.model, line(model)))
        })
        .collect();
    for change in &changes.tool_descriptions {
        let model = roles::ModelRef {
            profile: change.profile.clone(),
            provider_type: context
                .judge_config
                .providers
                .get(&change.profile)
                .map(|profile| profile.provider_type),
            model: change.route_model_id.clone(),
        };
        if judge_lines.contains(&line(&model)) {
            shared.insert(format!(
                "the description of `{}` is changed for {} ({}), used by {}",
                change.tool,
                model.model,
                line(&model),
                changes.affected_roles.join(", ")
            ));
        }
    }
    (!shared.is_empty()).then(|| {
        format!(
            "the judge ({}) shares a model family with what the candidate changes: {}",
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
    /// [`judge::compare_identity`] or [`judge::analysis_identity`]: what the
    /// judge was asked and told. A judgment made under another is not reused.
    #[serde(default)]
    identity: String,
    result: T,
    usage: Vec<RequestUsage>,
    judged_at: String,
}

struct JudgedPair {
    report: PairReport,
    usage: Vec<RequestUsage>,
}

/// What judging one baseline–candidate pair is asked about.
struct Pair<'a> {
    manifest: &'a Manifest,
    changes: &'a Changes,
    aspects: &'a [&'a str],
    index: u32,
    /// What the candidate replay really used of the description changes.
    effect: &'a breakdown::RunEffect,
}

async fn judge_pair(
    context: &mut Context<'_>,
    dir: &output::PinnedDir,
    pair: &Pair<'_>,
    baseline: &LoadedRun,
    candidate: &LoadedRun,
) -> JudgedPair {
    let Pair {
        manifest,
        changes,
        aspects,
        index,
        effect,
    } = *pair;
    let names = (baseline.summary.name(), candidate.summary.name());
    let mut usage = Vec::new();
    let mut errors = Vec::new();
    let mut orders: BTreeMap<&str, Verdicts> = BTreeMap::new();
    // A description that no run of the candidate froze makes no role's output
    // the effect of the change, so only the runs that used it are the focus.
    let applied = (!changes.tool_descriptions.is_empty()).then_some(&effect.applied_runs);
    let baseline_focus = Focus {
        roles: &changes.affected_roles,
        bindings: &changes.bindings,
        applied: None,
    };
    let candidate_focus = Focus {
        applied,
        ..baseline_focus
    };
    let sample = manifest.sample.as_ref().map(|pinned| pinned.hash.as_str());
    let compare_identity = judge::compare_identity(aspects, changes, sample);
    for order in [Order::BaselineFirst, Order::CandidateFirst] {
        let file = format!("pair-{index}-{}.json", order.as_str());
        let valid =
            |verdicts: &Verdicts| aspects.iter().all(|aspect| verdicts.contains_key(*aspect));
        let ((first, first_focus), (second, second_focus)) = match order {
            Order::BaselineFirst => ((baseline, &baseline_focus), (candidate, &candidate_focus)),
            Order::CandidateFirst => ((candidate, &candidate_focus), (baseline, &baseline_focus)),
        };
        let payload = serde_json::json!({
            "original_user_messages": manifest.original_turns,
            "problem": manifest.problem,
            "changes": judge::comparison_changes(changes, order),
            "A": side(first, first_focus),
            "B": side(second, second_focus),
        });
        match cached_or_judge(
            context,
            &manifest.thread_id,
            (dir, &file),
            (&names, &compare_identity),
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
    let payload = serde_json::json!({
        "problem": manifest.problem,
        "changes": changes,
        "affected_roles": changes.affected_roles,
        "applied": {
            "recorded": effect.observed,
            "descriptions": changes
                .tool_descriptions
                .iter()
                .zip(&effect.applied)
                .map(|(change, applied)| serde_json::json!({
                    "tool": change.tool,
                    "profile": change.profile,
                    "model": change.route_model_id,
                    "used_by_the_candidate": applied,
                }))
                .collect::<Vec<_>>(),
            "model_fallbacks": effect.fallbacks,
        },
        "baseline": process_side(baseline, &baseline_focus),
        "candidate": process_side(candidate, &candidate_focus),
    });
    let process = cached_or_judge(
        context,
        &manifest.thread_id,
        (dir, &format!("pair-{index}-process.json")),
        (&names, &judge::analysis_identity(changes, sample)),
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
    (dir, file): (&output::PinnedDir, &str),
    (names, identity): (&(String, String), &str),
    valid: impl Fn(&T) -> bool,
    ask: impl AsyncFnOnce(Judge) -> Judged<T>,
) -> Spent<T> {
    if !context.rejudge
        && let Ok(judgments) = dir.open_dir(output::JUDGMENTS_DIR)
        && let Ok(cached) = read_json_in::<Cached<T>>(&judgments, file)
        && (&cached.baseline, &cached.candidate) == (&names.0, &names.1)
        && cached.identity == identity
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
        identity: identity.to_owned(),
        result,
        usage: judged.usage,
        judged_at: output::timestamp(SystemTime::now()),
    };
    if let Err(error) = dir
        .ensure_dir(output::JUDGMENTS_DIR)
        .and_then(|judgments| judgments.write_json(file, &cached))
    {
        tracing::warn!(%error, path = %dir.join(output::JUDGMENTS_DIR).join(file).display(), "judgment could not be cached");
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
        // Null when any part is unknown: the priced part alone is not the cost.
        "usd": cost.is_complete().then_some(cost.usd),
        "priced_usd": cost.usd,
        "api_equivalent": cost.is_api_equivalent(),
        "complete": cost.is_complete(),
        "unpriced_models": unpriced_list(cost),
        "unreported_models": unreported_list(cost),
        "unrecorded_replays": cost.unrecorded,
        "unfinished_replays": cost.unfinished,
    })
}

/// The delegated runs a change is read from: the runs of the roles it can
/// reach, and, when a tool description changed, only those that really used it.
#[derive(Clone, Copy)]
struct Focus<'a> {
    /// Addresses of the roles the differences can reach.
    roles: &'a [String],
    /// Addresses of the roles whose binding differs, which need no description.
    bindings: &'a [String],
    /// The runs that used a changed description, when one is changed and this
    /// side is the one that carries it.
    applied: Option<&'a BTreeSet<String>>,
}

impl Focus<'_> {
    fn contains(&self, role: &str, run_id: &str) -> bool {
        let Some(address) = roles::address(role) else {
            return false;
        };
        self.roles.iter().any(|reached| reached == address)
            && self.applied.is_none_or(|applied| {
                applied.contains(run_id) || self.bindings.iter().any(|bound| bound == address)
            })
    }
}

/// The final output of the delegated runs, apart from the root's conversation.
/// Runs the changes reach (the focus) come first with a longer excerpt, so a
/// changed child role is judged on what it returned. Later runs win when over the cap.
fn delegated_outputs(record: &RunRecord, focus: &Focus<'_>) -> Vec<serde_json::Value> {
    let children = metrics::child_outputs(record);
    let (focused, other): (Vec<_>, Vec<_>) = children
        .iter()
        .partition(|child| focus.contains(&child.role, &child.run_id));
    let mut entries = Vec::new();
    for (children, max, chars, is_focus) in [
        (focused, DELEGATED_FOCUS_MAX, FOCUS_OUTPUT_CHARS, true),
        (other, DELEGATED_OTHER_MAX, OTHER_OUTPUT_CHARS, false),
    ] {
        let omitted = children.len().saturating_sub(max);
        entries.extend(children.iter().skip(omitted).map(|child| {
            serde_json::json!({
                "run_id": child.run_id,
                "role": child.role,
                "delegated_by": child.parent_role,
                "focus": is_focus,
                "phase": child.phase.map(|phase| format!("{phase:?}")),
                "final_output": child.final_output.as_deref().map(|text| clip(text, chars)),
            })
        }));
        if omitted > 0 {
            entries.push(serde_json::json!({
                "kind": "omitted",
                "focus": is_focus,
                "runs": omitted,
            }));
        }
    }
    entries
}

/// What the sample's machine checks found in a replay, for the judge: the
/// condition and the result of each, and whether the replay's record is a
/// complete account. Nothing in it names the configuration, so it can sit under
/// either position of a comparison. `null` when the thread declares no checks.
fn checks_view(run: &LoadedRun) -> serde_json::Value {
    let Some(grading) = &run.summary.checks else {
        return serde_json::Value::Null;
    };
    serde_json::json!({
        "quality": grading.quality().label(),
        "replay_record_complete": grading.validity.valid,
        "replay_record_issues": grading.validity.issues,
        "results": grading
            .results
            .iter()
            .map(|result| serde_json::json!({
                "check": result.description,
                "verdict": result.verdict,
                "observed": result.observed,
                "reason": result.reason,
            }))
            .collect::<Vec<_>>(),
    })
}

fn side(run: &LoadedRun, focus: &Focus<'_>) -> serde_json::Value {
    serde_json::json!({
        "status": run.summary.status,
        "status_reason": run.summary.reason,
        "conversation": bounded_conversation(
            &run.conversation,
            CONVERSATION_HEAD,
            CONVERSATION_TAIL,
            ENTRY_CHARS,
        ),
        "delegated_outputs": delegated_outputs(&run.record, focus),
        "diff": bounded_diff(&run.diff),
        "verification": run.summary.verification,
        "checks": checks_view(run),
        "metrics": compact_metrics(&run.summary.metrics),
        "cost": cost_value(&run.summary.cost),
    })
}

fn process_side(run: &LoadedRun, focus: &Focus<'_>) -> serde_json::Value {
    serde_json::json!({
        "status": run.summary.status,
        "status_reason": run.summary.reason,
        "metrics": compact_metrics(&run.summary.metrics),
        "checks": checks_view(run),
        "delegated_outputs": delegated_outputs(&run.record, focus),
        "timeline": metrics::timeline(&run.record),
    })
}

/// A cost as text: the amount when every part of it is known, and otherwise
/// `unknown` with the priced part, which is a lower bound and not the cost.
pub(crate) fn usd(cost: &Cost) -> String {
    let mut amount = format!("${:.4}", cost.usd);
    if cost.is_api_equivalent() {
        amount.push_str(" (API-equivalent)");
    }
    if cost.is_complete() {
        amount
    } else {
        format!("unknown (priced part {amount})")
    }
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
    for change in &report.changes.tool_descriptions {
        out.push_str(&format!(
            "- Tool description `{}` for {}/{} (the model is unchanged): {} → {}\n",
            change.tool,
            change.profile,
            change.route_model_id,
            change.baseline_hash.chars().take(12).collect::<String>(),
            change.candidate_hash.chars().take(12).collect::<String>()
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

    out.push_str(&checks_section(report));
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
    for name in &report.unfinished {
        out.push_str(&format!(
            "| {name} | not finished | - | - | - | - | - | - | - | - | unknown (no recorded end) | - |\n"
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
    out.push_str(&breakdown::markdown(&report.breakdown));

    if !report.pairs.is_empty() {
        out.push_str("\n## Pairwise verdicts\n\nCandidate wins / baseline wins per aspect, ties both orders gave, and pairs in which the orders disagreed (no result). Each pair is judged in both orders.\n\n| aspect | candidate | baseline | tie | orders disagreed |\n|---|---|---|---|---|\n");
        for tally in &report.tally {
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} |\n",
                tally.aspect,
                tally.candidate_wins,
                tally.baseline_wins,
                tally.ties,
                tally.order_disagreements
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
                    "- **{}**: {}{} — baseline first: {}; candidate first: {}\n",
                    result.aspect,
                    winner_name(result.winner),
                    match result.basis {
                        judge::Basis::Agreed => "",
                        judge::Basis::BothTie => " (tie in both orders)",
                        judge::Basis::OrderDisagreement => " (orders disagreed: no result)",
                    },
                    result.baseline_first.reason,
                    result.candidate_first.reason
                ));
            }
            if let Some(process) = &pair.process {
                out.push_str(&format!(
                    "\nProcess analysis:\n\n- Time and tokens: {}\n- Detours: {}\n- Delegation: {}\n- Affected roles: {}\n- Difference: {}\n",
                    process.time_and_tokens,
                    process.detours,
                    process.delegation,
                    process.affected_roles,
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

/// Per-thread win/loss/tie counts per aspect, from the candidate's side (the
/// last count is the pairs in which the judge's two orders disagreed), and
/// per-thread cost. Threads are listed side by side, never ranked overall.
pub fn cross_thread_table(reports: &[Report]) -> String {
    let aspects: Vec<&str> = judge::aspects(reports.iter().any(|report| report.problem.is_some()));
    let mut out = String::from("| thread |");
    for aspect in &aspects {
        out.push_str(&format!(" {aspect} W/L/T/D |"));
    }
    out.push_str(" baseline cost | candidate cost |\n|---|");
    out.push_str(&"---|".repeat(aspects.len() + 2));
    out.push('\n');
    for report in reports {
        out.push_str(&format!("| {} |", report.thread_id));
        for aspect in &aspects {
            match report.tally.iter().find(|tally| tally.aspect == *aspect) {
                Some(tally) if !report.pairs.is_empty() => out.push_str(&format!(
                    " {}/{}/{}/{} |",
                    tally.candidate_wins,
                    tally.baseline_wins,
                    tally.ties,
                    tally.order_disagreements
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
mod pinned_database_tests {
    use super::*;
    use crate::test_support::{usage, write_run_db};

    #[test]
    fn the_database_is_read_from_the_pinned_directory_even_if_its_place_is_swapped() {
        use std::os::unix::fs::symlink;

        // Given: a run directory with one request, pinned, and another database elsewhere.
        let root = tempfile::tempdir().unwrap();
        let run = root.path().join("run");
        let moved = root.path().join("moved");
        let outside = root.path().join("outside");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        write_run_db(
            &run.join(output::DATABASE_FILE),
            &[],
            vec![usage("own", "claude", "claude-sonnet-9", "agent", 10)],
        );
        write_run_db(
            &outside.join(output::DATABASE_FILE),
            &[],
            vec![
                usage("a", "claude", "claude-sonnet-9", "agent", 10),
                usage("b", "claude", "claude-sonnet-9", "agent", 10),
            ],
        );
        let pinned = output::PinnedDir::open_path(&run).unwrap();

        // When: the directory is replaced by a link to the other place, and then read.
        std::fs::rename(&run, &moved).unwrap();
        symlink(&outside, &run).unwrap();
        let record = load_database(&pinned).unwrap();

        // Then: the pinned directory's own database was read.
        assert_eq!(record.agent_requests().count(), 1);
    }

    #[test]
    fn a_database_that_is_a_link_or_missing_gives_an_empty_record() {
        // Given: a run directory whose database is a link to a real one, and an empty one.
        let root = tempfile::tempdir().unwrap();
        let run = root.path().join("run");
        let empty = root.path().join("empty");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::create_dir_all(&empty).unwrap();
        let real = root.path().join("real.db");
        write_run_db(
            &real,
            &[],
            vec![usage("a", "claude", "claude-sonnet-9", "agent", 10)],
        );
        std::os::unix::fs::symlink(&real, run.join(output::DATABASE_FILE)).unwrap();

        // When / Then: neither is followed or required.
        for dir in [&run, &empty] {
            let pinned = output::PinnedDir::open_path(dir).unwrap();
            assert_eq!(load_database(&pinned).unwrap().agent_requests().count(), 0);
        }
    }
}

#[cfg(test)]
mod tests;
