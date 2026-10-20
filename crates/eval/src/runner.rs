//! Run an evaluation: every baseline replay, then every candidate replay, each
//! in its own clone with its own runtime and database (ADR 0036 §4, §6).

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

use config::Config;
use config::agent_categories::CategoryId;
use event_bus::EventBus;
use recorder::pricing::UsagePricing;
use recorder::storage_bridge::{OwnedStorageBridge, StorageBridge};
use routing::EnvLookup;
use runtime::{
    AgentModel, ExecutionPolicy, ModelSource, ProjectTrust, Role, RunConfig, RunStore,
    RuntimeComposition, compose_runtime, production_executor_with_config,
};
use sandbox::DirectSandbox;
use sandbox::credential::CredentialStore;
use tools::ToolExecutor;

use crate::driver::{self, Replay, ReplayOutcome, Status};
use crate::output::{self, Manifest, Outcome, RunEntry, SimulatorSummary, StartSummary};
use crate::simulator::UserSimulator;
use crate::source::{OriginalThread, SourceError};
use crate::workspace::{self, CommitIdentity, Workspace};

const BUS_CAPACITY: usize = 65_536;
const BRIDGE_FLUSH_INTERVAL: Duration = Duration::from_secs(60);
const OVERLAY_FORBIDDEN_KEYS: [&str; 3] = ["eval", "tools", "comment_checker"];
pub const BASELINE: &str = "baseline";
pub const CANDIDATE: &str = "candidate";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EvalArgs {
    pub thread_id: String,
    pub candidate: Option<PathBuf>,
    pub repeat: Option<u32>,
    pub output: Option<PathBuf>,
    pub db: Option<PathBuf>,
    pub user_config_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Isolation {
    Production,
    /// No sandbox around tools; only for tests.
    DirectUnchecked,
}

/// Builds a model for one replay, given that replay's event bus.
pub type ModelFactory = Arc<dyn Fn(&Arc<EventBus>) -> Arc<dyn AgentModel> + Send + Sync>;

/// Models that replace the configured providers, for tests.
#[derive(Clone)]
pub struct FixedModels {
    pub agent: ModelFactory,
    pub simulator: ModelFactory,
}

pub struct RunnerDeps {
    pub env: Arc<dyn EnvLookup>,
    pub isolation: Isolation,
    pub models: Option<FixedModels>,
    /// `None` opens `<user config dir>/credentials`.
    pub credentials: Option<Arc<dyn CredentialStore>>,
    pub catalog: Option<Arc<catalog::ModelCatalog>>,
}

#[derive(Debug, thiserror::Error)]
pub enum EvalError {
    #[error("{0}")]
    Usage(String),
    #[error(transparent)]
    Config(#[from] config::ConfigError),
    #[error("cannot open the source database {}: {source}", path.display())]
    Database {
        path: PathBuf,
        source: storage::StorageError,
    },
    #[error(transparent)]
    Source(#[from] SourceError),
    #[error("the candidate overlay is unusable: {0}")]
    Overlay(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Credential(#[from] sandbox::CredentialError),
    #[error("credential initialization task failed: {0}")]
    CredentialTask(#[from] tokio::task::JoinError),
}

#[derive(Debug, Clone)]
pub struct RunReport {
    pub label: String,
    pub index: u32,
    pub dir: PathBuf,
    pub outcome: Outcome,
}

#[derive(Debug, Clone)]
pub struct Evaluation {
    pub dir: PathBuf,
    pub runs: Vec<RunReport>,
}

struct Context {
    thread: OriginalThread,
    user_config_dir: Option<PathBuf>,
    budget: config::EvalBudgetConfig,
    simulator: Config,
    identity: CommitIdentity,
    trust: ProjectTrust,
    credentials: Arc<dyn CredentialStore>,
    deps: RunnerDeps,
}

/// Replay the thread `repeat` times under the user's configuration and then,
/// with a candidate overlay, `repeat` times under the candidate, writing every
/// result below the output directory. `on_run` sees each replay as it ends.
///
/// # Errors
/// Fails before any replay when the thread, configuration, overlay or output
/// directory is unusable, and when a result cannot be written. A replay that
/// fails is reported in its outcome instead.
pub async fn run(
    args: EvalArgs,
    deps: RunnerDeps,
    mut on_run: impl FnMut(&RunReport),
) -> Result<Evaluation, EvalError> {
    let user_config_dir = args
        .user_config_dir
        .clone()
        .or_else(config::user_config_dir);
    let needs_user_dir = |what: &str| {
        EvalError::Usage(format!(
            "cannot resolve a user config directory for {what}; pass --user-config <dir>"
        ))
    };
    let source_db = match (&args.db, &user_config_dir) {
        (Some(db), _) => db.clone(),
        (None, Some(dir)) => dir.join("evorch-events.db"),
        (None, None) => return Err(needs_user_dir("the event database")),
    };
    let thread = {
        let database = storage::Database::open_read_only(&source_db).map_err(|source| {
            EvalError::Database {
                path: source_db.clone(),
                source,
            }
        })?;
        OriginalThread::load(&database, &args.thread_id)?
    };
    let base = load_config(user_config_dir.as_deref(), None, None)?;
    let overlay = match &args.candidate {
        Some(path) => Some(read_overlay(path, user_config_dir.as_deref())?),
        None => None,
    };
    let repeat = args.repeat.unwrap_or(base.eval.repeat_target);
    if repeat == 0 {
        return Err(EvalError::Usage("--repeat must be at least 1".into()));
    }
    let dir = match (&args.output, &user_config_dir) {
        (Some(dir), _) => dir.clone(),
        (None, Some(user)) => output::default_dir(user, &thread.thread_id, SystemTime::now()),
        (None, None) => return Err(needs_user_dir("the output directory")),
    };
    output::create_dir(&dir)?;
    let credentials = match &deps.credentials {
        Some(store) => Arc::clone(store),
        None => {
            let store = user_config_dir
                .as_ref()
                .ok_or_else(|| needs_user_dir("the credential store"))?
                .join("credentials");
            tokio::task::spawn_blocking(move || sandbox::credential::open_default(store)).await??
        }
    };
    let (simulator, simulator_summary) = simulator_config(&base);
    let configurations: Vec<(&str, Option<&toml::Value>)> = std::iter::once((BASELINE, None))
        .chain(overlay.as_ref().map(|(_, value)| (CANDIDATE, Some(value))))
        .collect();
    let mut manifest = Manifest {
        thread_id: thread.thread_id.clone(),
        source_db,
        start: StartSummary::of(&thread),
        candidate_overlay: overlay.as_ref().map(|(text, _)| text.clone()),
        configurations: configurations
            .iter()
            .map(|(label, _)| (*label).to_owned())
            .collect(),
        repeat,
        simulator: simulator_summary,
        budget: base.eval.budget.clone(),
        started_at: output::timestamp(SystemTime::now()),
        finished_at: None,
        runs: Vec::new(),
    };
    let manifest_path = dir.join(output::MANIFEST_FILE);
    output::write_json(&manifest_path, &manifest)?;
    let context = Context {
        trust: mirrored_trust(user_config_dir.as_deref(), &thread.project_root),
        identity: CommitIdentity::from_host(&thread.source_toplevel),
        thread,
        user_config_dir,
        budget: base.eval.budget.clone(),
        simulator,
        credentials,
        deps,
    };
    let mut runs = Vec::new();
    for (label, overlay) in configurations {
        for index in 1..=repeat {
            let run_dir = dir.join("runs").join(format!("{label}-{index}"));
            let nonce = nonce()?;
            manifest.runs.push(RunEntry {
                label: label.to_owned(),
                index,
                dir: format!("runs/{label}-{index}"),
                nonce: nonce.clone(),
                started_at: output::timestamp(SystemTime::now()),
                finished_at: None,
                status: None,
            });
            output::write_json(&manifest_path, &manifest)?;
            let outcome = replay_once(&context, overlay, &run_dir, &nonce).await?;
            if let Some(entry) = manifest.runs.last_mut() {
                entry.finished_at = Some(output::timestamp(SystemTime::now()));
                entry.status = Some(outcome.status);
            }
            output::write_json(&manifest_path, &manifest)?;
            let report = RunReport {
                label: label.to_owned(),
                index,
                dir: run_dir,
                outcome,
            };
            on_run(&report);
            runs.push(report);
        }
    }
    manifest.finished_at = Some(output::timestamp(SystemTime::now()));
    output::write_json(&manifest_path, &manifest)?;
    Ok(Evaluation { dir, runs })
}

fn load_config(
    user_config_dir: Option<&Path>,
    project_dir: Option<&Path>,
    overlay: Option<&toml::Value>,
) -> Result<Config, config::ConfigError> {
    Config::load(&config::LoadOptions {
        project_dir: project_dir.map(Path::to_path_buf),
        user_config_dir: user_config_dir.map(Path::to_path_buf),
        cli_overrides: overlay.cloned(),
        read_env: false,
        ..config::LoadOptions::default()
    })
}

fn read_overlay(
    path: &Path,
    user_config_dir: Option<&Path>,
) -> Result<(String, toml::Value), EvalError> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| EvalError::Overlay(format!("{}: {error}", path.display())))?;
    let value: toml::Value = toml::from_str(&text)
        .map_err(|error| EvalError::Overlay(format!("{}: {error}", path.display())))?;
    let table = value
        .as_table()
        .ok_or_else(|| EvalError::Overlay("the overlay must be a TOML table".into()))?;
    if let Some(key) = OVERLAY_FORBIDDEN_KEYS
        .iter()
        .find(|key| table.contains_key(**key))
    {
        return Err(EvalError::Overlay(format!(
            "`{key}` cannot be changed by a candidate"
        )));
    }
    load_config(user_config_dir, None, Some(&value))
        .map_err(|error| EvalError::Overlay(error.to_string()))?;
    Ok((text, value))
}

/// The simulated user runs on the user's configuration, never the candidate's,
/// with its binding in the worker slot it is invoked through.
fn simulator_config(base: &Config) -> (Config, SimulatorSummary) {
    let mut config = base.clone();
    let summary = match &base.eval.user_simulator {
        Some(binding) => {
            config.agents.worker.base = binding.clone();
            SimulatorSummary {
                source: "eval.user_simulator".into(),
                binding: binding.clone(),
            }
        }
        None => SimulatorSummary {
            source: "agents.worker (eval.user_simulator is unset)".into(),
            binding: base.agents.worker.base.clone(),
        },
    };
    (config, summary)
}

/// The original project's trust in the GUI sidebar, or approved when the
/// sidebar does not know it: the replay runs in a clone the user asked for.
fn mirrored_trust(user_config_dir: Option<&Path>, project_root: &Path) -> ProjectTrust {
    let Some(sidebar) = user_config_dir
        .map(|dir| dir.join("sidebar.json"))
        .and_then(|path| workspace_ui::load_sidebar(&path).ok())
    else {
        return ProjectTrust::Approved;
    };
    let canonical = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let root = canonical(project_root);
    match sidebar
        .projects
        .iter()
        .find(|project| canonical(&project.repo_root) == root)
        .map(|project| project.trust)
    {
        Some(workspace_ui::TrustState::Unapproved) => ProjectTrust::Unapproved,
        _ => ProjectTrust::Approved,
    }
}

fn nonce() -> std::io::Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| std::io::Error::other(error.to_string()))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn because<E: std::fmt::Display>(what: &'static str) -> impl FnOnce(E) -> String {
    move |error| format!("{what}: {error}")
}

async fn replay_once(
    context: &Context,
    overlay: Option<&toml::Value>,
    run_dir: &Path,
    nonce: &str,
) -> Result<Outcome, EvalError> {
    std::fs::create_dir_all(run_dir)?;
    let prepared = workspace::prepare(
        &context.thread.source_toplevel,
        &context.thread.snapshot,
        &run_dir.join("workspace"),
        &context.identity,
    );
    let workspace = match prepared {
        Ok(workspace) => workspace,
        Err(error) => {
            let outcome = Outcome {
                status: Status::PrepareFailed,
                reason: error.to_string(),
                user_turns: 0,
                tokens: 0,
                tool_calls: 0,
                elapsed_ms: 0,
                denied_requests: Vec::new(),
                pushed_refs: Vec::new(),
            };
            output::write_run(run_dir, &outcome, &[], None)?;
            return Ok(outcome);
        }
    };
    let replay = match drive_in(context, overlay, &workspace, run_dir, nonce).await {
        Ok(replay) => replay,
        Err(reason) => ReplayOutcome {
            status: Status::Failed,
            reason,
            user_turns: 0,
            tokens: 0,
            tool_calls: 0,
            elapsed_ms: 0,
            denied_requests: Vec::new(),
            conversation: Vec::new(),
        },
    };
    let (status, reason, diff, pushed_refs) = match workspace.finish() {
        Ok(result) => (
            replay.status,
            replay.reason,
            Some(result.diff),
            result.pushed_refs,
        ),
        Err(error) => (
            Status::Failed,
            format!(
                "{}; the final workspace could not be read: {error}",
                replay.reason
            ),
            None,
            Vec::new(),
        ),
    };
    let outcome = Outcome {
        status,
        reason,
        user_turns: replay.user_turns,
        tokens: replay.tokens,
        tool_calls: replay.tool_calls,
        elapsed_ms: replay.elapsed_ms,
        denied_requests: replay.denied_requests,
        pushed_refs,
    };
    output::write_run(run_dir, &outcome, &replay.conversation, diff.as_deref())?;
    Ok(outcome)
}

/// Compose a fresh runtime for the prepared clone, drive the replay, and
/// close the runtime before the run's database is flushed and released.
async fn drive_in(
    context: &Context,
    overlay: Option<&toml::Value>,
    workspace: &Workspace,
    run_dir: &Path,
    nonce: &str,
) -> Result<ReplayOutcome, String> {
    let project = workspace.project_dir.clone();
    let role = context.thread.role;
    let mut config = load_config(context.user_config_dir.as_deref(), Some(&project), overlay)
        .map_err(because("configuration"))?;
    config.tools.browser.enabled = false;
    let bus = Arc::new(EventBus::new(BUS_CAPACITY));
    let storage_config = storage::StorageConfig {
        db_path: run_dir.join(output::DATABASE_FILE),
        ..storage::StorageConfig::default()
    };
    let storage =
        storage::Storage::open(storage_config.clone()).map_err(because("run database"))?;
    let pricing = Arc::new(RwLock::new(UsagePricing::new(
        config.providers.clone(),
        context.deps.catalog.clone(),
    )));
    let mut bridge = OwnedStorageBridge::spawn(
        Arc::clone(&bus),
        storage,
        |handle| StorageBridge::new(handle, "eval").with_usage_ledger(pricing),
        BRIDGE_FLUSH_INTERVAL,
    )
    .map_err(because("run database"))?;
    let outcome = {
        let executor = match context.deps.isolation {
            Isolation::Production => production_executor_with_config(
                Arc::clone(&bus),
                &ExecutionPolicy::for_role(role),
                project.clone(),
                &config,
            )
            .map_err(because("tool sandbox"))?,
            Isolation::DirectUnchecked => Arc::new(ToolExecutor::with_standard_tools_in(
                Arc::clone(&bus),
                Arc::new(DirectSandbox::new_unchecked()),
                Some(project.clone()),
            )),
        };
        let model_source = match &context.deps.models {
            Some(models) => ModelSource::Fixed((models.agent)(&bus)),
            None => ModelSource::Configured,
        };
        let runtime = compose_runtime(RuntimeComposition {
            config: &config,
            user_config_dir: context.user_config_dir.clone(),
            bus: Arc::clone(&bus),
            executor,
            credential_store: Arc::clone(&context.credentials),
            env: Arc::clone(&context.deps.env),
            model_source,
            workspace: None,
        })
        .map_err(because("model composition"))?
        .runtime
        .with_run_store(
            RunStore::open(&storage_config, bridge.handle()).map_err(because("run database"))?,
        );
        let runtime = match context.deps.isolation {
            Isolation::Production => runtime.with_sandbox_root(project.clone()),
            Isolation::DirectUnchecked => runtime,
        }
        .with_shell_environment(vec![("CARGO_NET_OFFLINE".into(), "true".into())])
        .with_system_prompt_preamble(format!("Run identifier: {nonce}"));
        runtime
            .set_project_root(project.clone())
            .map_err(because("project root"))?;
        runtime.set_project_trust(project.clone(), context.trust);
        runtime.set_sandbox_escalation(config::EscalationApproval::Off, false);
        runtime.set_web_tools_enabled(true);
        let simulator =
            UserSimulator::new(simulator_model(context, &bus)?, &context.thread.thread_id);
        driver::drive(Replay {
            runtime: &runtime,
            bus: &bus,
            thread: &context.thread,
            simulator: &simulator,
            budget: &context.budget,
            chat: RunConfig {
                conversation: true,
                interactive: true,
                keep_alive: true,
                category: (role == Role::Worker).then(|| CategoryId::Conversation.to_string()),
                project_root: Some(project),
                ..RunConfig::default()
            },
        })
        .await
    };
    if let Err(error) = bridge.flush() {
        tracing::warn!(%error, "evaluation run database flush failed");
    }
    bridge.shutdown();
    Ok(outcome)
}

fn simulator_model(context: &Context, bus: &Arc<EventBus>) -> Result<Arc<dyn AgentModel>, String> {
    if let Some(models) = &context.deps.models {
        return Ok((models.simulator)(bus));
    }
    let user_dir = context
        .user_config_dir
        .clone()
        .or_else(config::user_config_dir);
    if user_dir.is_none()
        && context.simulator.providers.values().any(|profile| {
            profile.provider_type == config::ProviderTypeConfig::AnthropicSubscription
        })
    {
        return Err(
            "user simulator: Claude installation identity requires a user configuration directory"
                .into(),
        );
    }
    let model = runtime::compose::compose_routed_model(
        &context.simulator,
        routing::ComposeDeps {
            credential_store: Arc::clone(&context.credentials),
            event_bus: Some(Arc::clone(bus)),
            env: Arc::clone(&context.deps.env),
            catalog: model::ModelCatalog::new(),
            factory: routing::factory::FactoryOptions {
                claude_installation_id_path: user_dir
                    .map(|dir| dir.join(providers::provider::claude::CLAUDE_INSTALLATION_ID_FILE)),
                ..Default::default()
            },
        },
    )
    .map_err(|error| format!("user simulator: {error}"))?;
    Ok(model)
}

#[cfg(test)]
mod tests;
