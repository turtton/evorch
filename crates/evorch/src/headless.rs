//! headless 実行エントリ (issue #79)。
//!
//! provider composition root ([`runtime::compose_runtime`]) を CLI から共有する
//! 最小実行経路を提供する。

use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use event_bus::{AgentRunPhase, Event, EventBus, EventKind, LifecycleEvent, ToolEvent};
use recorder::pricing::UsagePricing;
use recorder::storage_bridge::{OwnedStorageBridge, StorageBridge};
use routing::EnvLookup;
use runtime::{
    ExecutionPolicy, ModelSource, Role, RunConfig, RunId, RunStore, RuntimeComposition,
    compose_runtime, production_executor_with_config,
};
use sandbox::credential::{CredentialStore, open_default};
use sandbox::{CredentialError, DirectSandbox};
use tools::ToolExecutor;

/// `parse_args` の使い方テキスト。
pub const USAGE: &str = "usage: evorch run --project <dir> --role <worker|orchestrator|explorer|reviewer|web_researcher|planner|advisor> --prompt <text> [--user-config <dir>] [--web-tools-enabled <true|false>] [--run-js-enabled <true|false>] [--db <path>]";

/// How often the persistence bridge flushes while a `--db` run is live. The run
/// end flushes explicitly, so this only bounds what an abnormal exit can lose.
const BRIDGE_FLUSH_INTERVAL: Duration = Duration::from_secs(5);

/// `evorch run` の引数。
#[derive(Debug, Clone)]
pub struct HeadlessArgs {
    pub project_dir: PathBuf,
    pub role: Role,
    pub prompt: String,
    pub user_config_dir: Option<PathBuf>,
    /// Optional override of the saved Web tool availability switch.
    pub web_tools_enabled: Option<bool>,
    /// Optional override of `[tools.run_js] enabled` (ADR 0039).
    pub run_js_enabled: Option<bool>,
    /// SQLite database that receives the run's events, run state and usage
    /// ledger (readable with `evorch inspect --db <path> run <run-id>`).
    /// `None` keeps the run unpersisted.
    pub db_path: Option<PathBuf>,
    /// models.dev cache directory; not a CLI option. `None` uses
    /// [`catalog::default_cache_dir`].
    pub model_catalog_dir: Option<PathBuf>,
}

/// tool 実行の sandbox 方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxChoice {
    /// production 構成 (`production_executor`、fail-closed)。
    Production,
    /// 隔離なしの実行 (非 production / テスト専用の明示的 opt-out)。
    DirectUnchecked,
}

/// headless run の結果。
#[derive(Debug)]
pub struct HeadlessOutcome {
    pub run_id: RunId,
    pub phase: AgentRunPhase,
    pub final_text: Option<String>,
}

/// headless 実行の失敗。
#[derive(Debug, thiserror::Error)]
pub enum HeadlessError {
    #[error("{0}")]
    Usage(String),
    #[error(transparent)]
    Config(#[from] config::ConfigError),
    #[error(transparent)]
    Composition(#[from] runtime::CompositionError),
    #[error(transparent)]
    Runtime(#[from] runtime::RuntimeError),
    #[error(transparent)]
    Credential(#[from] CredentialError),
    #[error("run database: {0}")]
    Database(String),
    #[error("credential initialization task failed: {0}")]
    CredentialTask(#[from] tokio::task::JoinError),
    /// The run ended its turn waiting for an answer to a question it asked the user.
    /// A non-interactive run has no one to ask, so it is stopped instead of waiting
    /// forever.
    #[error(
        "run {run_id} is waiting for a user answer, but a headless run is non-interactive and nobody can answer; it was stopped. Question: {questions}"
    )]
    WaitingForInput { run_id: String, questions: String },
}

/// Exit status of `evorch run` when the run was stopped because it waited for a user
/// answer (see [`HeadlessError::WaitingForInput`]). Other failures exit with 1.
pub const EXIT_WAITING_FOR_INPUT: u8 = 3;

/// The exit status `evorch run` ends with for `error`.
pub fn exit_code(error: &HeadlessError) -> u8 {
    match error {
        HeadlessError::WaitingForInput { .. } => EXIT_WAITING_FOR_INPUT,
        _ => 1,
    }
}

/// コマンドライン引数を手でパースする (clap 不使用)。
///
/// # Errors
/// サブコマンドが `run` 以外、フラグが未知、必須フラグやその値が欠落、
/// role が既知名でない、`--project` を絶対パスにできない (空など) 場合に
/// [`HeadlessError::Usage`] を返す。
pub fn parse_args(argv: impl Iterator<Item = String>) -> Result<HeadlessArgs, HeadlessError> {
    let mut argv = argv;
    match argv.next() {
        Some(command) if command == "run" => {}
        _ => return Err(usage_error()),
    }

    let mut project = None;
    let mut role = None;
    let mut prompt = None;
    let mut user_config = None;
    let mut web_tools_enabled = None;
    let mut run_js_enabled = None;
    let mut db = None;
    while let Some(flag) = argv.next() {
        let Some(value) = argv.next() else {
            return Err(usage_error());
        };
        match flag.as_str() {
            "--project" => project = Some(value),
            "--role" => role = Some(value),
            "--prompt" => prompt = Some(value),
            "--user-config" => user_config = Some(value),
            "--web-tools-enabled" => web_tools_enabled = Some(parse_bool(&value)?),
            "--run-js-enabled" => run_js_enabled = Some(parse_bool(&value)?),
            "--db" => db = Some(value),
            _ => return Err(usage_error()),
        }
    }

    let (Some(project_dir), Some(role_text), Some(prompt)) = (project, role, prompt) else {
        return Err(usage_error());
    };
    let Some(role) = parse_role(&role_text) else {
        return Err(usage_error());
    };
    // 設定・sandbox・file tool はどれもこのディレクトリにパスを結合するので、
    // ここで一度だけ現在の作業ディレクトリ基準の絶対パスに固定する。
    let project_dir = std::path::absolute(&project_dir).map_err(|_| usage_error())?;

    let db_path = db
        .map(|path| std::path::absolute(path).map_err(|_| usage_error()))
        .transpose()?;

    Ok(HeadlessArgs {
        project_dir,
        role,
        prompt,
        user_config_dir: user_config.map(PathBuf::from),
        web_tools_enabled,
        run_js_enabled,
        db_path,
        model_catalog_dir: None,
    })
}

fn parse_bool(value: &str) -> Result<bool, HeadlessError> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(usage_error()),
    }
}

fn usage_error() -> HeadlessError {
    HeadlessError::Usage(USAGE.to_string())
}

fn parse_role(text: &str) -> Option<Role> {
    match text.to_ascii_lowercase().as_str() {
        "orchestrator" => Some(Role::Orchestrator),
        "explorer" => Some(Role::Explorer),
        "worker" => Some(Role::Worker),
        "reviewer" => Some(Role::Reviewer),
        "web_researcher" | "webresearcher" => Some(Role::WebResearcher),
        "planner" => Some(Role::Planner),
        "advisor" => Some(Role::Advisor),
        _ => None,
    }
}

/// `--db` で開く永続化先。bridge が bus のイベントと usage ledger を書き、
/// `storage_config` から開く [`RunStore`] が run の文脈 (`run_contexts`) を同じ DB に書く。
struct Persistence {
    bridge: OwnedStorageBridge,
    storage_config: storage::StorageConfig,
}

/// headless run を 1 回実行する。
///
/// `Config::load` → `EventBus` → executor → credential store →
/// [`compose_runtime`] → delegate → `wait` の単一経路で構成し、
/// provider composition root を将来の GUI 経路と共有する。
/// `db_path` があるときは run の終了 (成功・失敗とも) で永続化を flush して閉じる。
///
/// # Errors
/// 設定読み込み・composition・sandbox 構築・実行の失敗時に
/// [`HeadlessError`] の対応 variant を返す。
pub async fn run_headless(
    args: HeadlessArgs,
    env: Arc<dyn EnvLookup>,
    sandbox: SandboxChoice,
) -> Result<HeadlessOutcome, HeadlessError> {
    let config = config::Config::load(&config::LoadOptions {
        project_dir: Some(args.project_dir.clone()),
        user_config_dir: args.user_config_dir.clone(),
        read_env: false,
        ..config::LoadOptions::default()
    })?;
    let bus = Arc::new(EventBus::new(256));
    // The bridge subscribes before anything can emit, so the stored run starts
    // at its first event.
    let mut persistence = match &args.db_path {
        Some(path) => Some(open_persistence(path, &args, &config, &bus).await?),
        None => None,
    };
    let outcome = drive(&args, env, sandbox, &config, &bus, persistence.as_ref()).await;
    if let Some(persistence) = persistence.as_mut() {
        // Flush even when the run failed: the events before the failure are the evidence.
        if let Err(error) = persistence.bridge.flush() {
            eprintln!("run database flush failed: {error}");
        }
        persistence.bridge.shutdown();
    }
    outcome
}

async fn open_persistence(
    path: &std::path::Path,
    args: &HeadlessArgs,
    config: &config::Config,
    bus: &Arc<EventBus>,
) -> Result<Persistence, HeadlessError> {
    let database_error = |error: &dyn std::fmt::Display| HeadlessError::Database(error.to_string());
    let storage_config = storage::StorageConfig {
        db_path: path.to_path_buf(),
        ..storage::StorageConfig::default()
    };
    let storage = storage::Storage::open(storage_config.clone()).map_err(|e| database_error(&e))?;
    // Without a catalog the ledger prices from the provider configuration only.
    let catalog_dir = args
        .model_catalog_dir
        .clone()
        .unwrap_or_else(catalog::default_cache_dir);
    let catalog = match catalog::ModelCatalog::load_or_refresh(&catalog_dir).await {
        Ok(catalog) => Some(Arc::new(catalog)),
        Err(error) => {
            eprintln!("model catalog unavailable; usage is priced from config only: {error}");
            None
        }
    };
    let pricing = Arc::new(RwLock::new(UsagePricing::new(
        config.providers.clone(),
        catalog,
    )));
    let bridge = OwnedStorageBridge::spawn(
        Arc::clone(bus),
        storage,
        |handle| StorageBridge::new(handle, "headless").with_usage_ledger(pricing),
        BRIDGE_FLUSH_INTERVAL,
    )
    .map_err(|e| database_error(&e))?;
    Ok(Persistence {
        bridge,
        storage_config,
    })
}

async fn drive(
    args: &HeadlessArgs,
    env: Arc<dyn EnvLookup>,
    sandbox: SandboxChoice,
    config: &config::Config,
    bus: &Arc<EventBus>,
    persistence: Option<&Persistence>,
) -> Result<HeadlessOutcome, HeadlessError> {
    let web_tools_enabled = args
        .web_tools_enabled
        .unwrap_or(config.sandbox.web_tools_enabled);
    let approval_receiver = bus.subscribe();
    let executor: Arc<ToolExecutor> = match sandbox {
        SandboxChoice::Production => production_executor_with_config(
            Arc::clone(bus),
            &ExecutionPolicy::for_role(args.role),
            args.project_dir.clone(),
            config,
        )?,
        SandboxChoice::DirectUnchecked => Arc::new(
            ToolExecutor::with_standard_tools(
                Arc::clone(bus),
                Arc::new(DirectSandbox::new_unchecked()),
            )
            .with_web_tools()
            .and_then(|executor| executor.with_browser_tools(&config.tools.browser))
            .map_err(|error| runtime::RuntimeError::NetworkGuard {
                detail: error.to_string(),
            })?,
        ),
    };
    let credential_args = args.clone();
    let credential_store =
        tokio::task::spawn_blocking(move || open_credential_store(&credential_args)).await??;

    let composed = compose_runtime(RuntimeComposition {
        model_catalog_dir: args.model_catalog_dir.clone(),
        user_config_dir: args.user_config_dir.clone(),
        config,
        bus: Arc::clone(bus),
        executor,
        credential_store,
        env,
        model_source: ModelSource::Configured,
        workspace: None,
    })?;

    let mut runtime = match sandbox {
        SandboxChoice::Production => composed.runtime.with_sandbox_root(args.project_dir.clone()),
        SandboxChoice::DirectUnchecked => composed.runtime,
    };
    if let Some(persistence) = persistence {
        let store = RunStore::open(&persistence.storage_config, persistence.bridge.handle())
            .map_err(|error| HeadlessError::Database(error.to_string()))?;
        runtime = runtime.with_run_store(store);
    }
    runtime.set_web_tools_enabled(web_tools_enabled);
    if let Some(enabled) = args.run_js_enabled {
        runtime.set_run_js_settings(config::RunJsConfig {
            enabled,
            ..runtime.run_js_settings()
        });
    }
    let approval_task = tokio::spawn(serve_approvals(approval_receiver, Arc::clone(bus)));
    // Subscribed before the run starts, so no question or phase change is missed.
    let watcher = bus.subscribe();
    let run_id = runtime.delegate_background(args.role, args.prompt.clone(), RunConfig::default());
    if persistence.is_some() {
        eprintln!("run_id: {run_id}");
    }
    let waited = wait_unless_asking(&runtime, run_id, watcher).await;
    approval_task.abort();
    let phase = match waited {
        Ok(phase) => phase?,
        Err(questions) => {
            // Stopped, not cancelled: the run stays resumable from its database.
            let _ = runtime.stop(run_id, runtime::StopScope::Subtree);
            let _ = runtime.wait(run_id).await;
            return Err(HeadlessError::WaitingForInput {
                run_id: run_id.to_string(),
                questions: questions.join(" / "),
            });
        }
    };
    let final_text = runtime.run_result(run_id)?;

    Ok(HeadlessOutcome {
        run_id,
        phase,
        final_text,
    })
}

/// Waits for the run to end. A run that ends its turn with a blocking question to the
/// user pending goes to `Waiting` for an answer; nothing in a headless run can give
/// one, so that is reported as `Err` with the pending questions instead of waiting on.
async fn wait_unless_asking(
    runtime: &runtime::AgentRuntime,
    run_id: RunId,
    mut events: event_bus::EventReceiver,
) -> Result<Result<AgentRunPhase, runtime::RuntimeError>, Vec<String>> {
    let id = run_id.to_string();
    let mut pending: Vec<(String, String)> = Vec::new();
    let wait = runtime.wait(run_id);
    tokio::pin!(wait);
    loop {
        tokio::select! {
            phase = &mut wait => return Ok(phase),
            event = events.recv() => {
                let event = match event {
                    Ok(event) => event,
                    Err(event_bus::RecvError::Lagged(_)) => continue,
                    // No more events: only the run's end is left to wait for.
                    Err(event_bus::RecvError::Closed) => return Ok((&mut wait).await),
                };
                match event.kind {
                    EventKind::Tool(ToolEvent::UserQuestionUpdated { question })
                        if question.run_id == id =>
                    {
                        pending.retain(|(known, _)| *known != question.id);
                        if question.blocking && question.answer.is_none() {
                            pending.push((question.id, question.title));
                        }
                    }
                    EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                        run_id: changed,
                        to: AgentRunPhase::Waiting,
                        ..
                    }) if changed == id && !pending.is_empty() => {
                        return Err(pending.into_iter().map(|(_, title)| title).collect());
                    }
                    _ => {}
                }
            }
        }
    }
}

fn open_credential_store(args: &HeadlessArgs) -> Result<Arc<dyn CredentialStore>, HeadlessError> {
    let dir = args
        .user_config_dir
        .clone()
        .or_else(config::user_config_dir)
        .map(|dir| dir.join("credentials"))
        .ok_or_else(|| {
            HeadlessError::Usage(
                "cannot resolve a user config directory for the credential store; pass --user-config <dir>"
                    .to_string(),
            )
        })?;
    Ok(open_default(dir)?)
}

/// Answer tool approval requests on the terminal for a headless run.
async fn serve_approvals(mut receiver: event_bus::EventReceiver, bus: Arc<EventBus>) {
    loop {
        let event = match receiver.recv().await {
            Ok(event) => event,
            Err(event_bus::RecvError::Lagged(_)) => continue,
            Err(event_bus::RecvError::Closed) => return,
        };
        let EventKind::Tool(ToolEvent::ApprovalRequested {
            tool_name,
            call_id,
            input,
        }) = event.kind
        else {
            continue;
        };
        let display_call_id = call_id.clone();
        let approved = tokio::task::spawn_blocking(move || {
            use std::io::Write;
            if let Some(input) = input {
                eprintln!("{input}");
            }
            eprint!("Approve {tool_name} [{display_call_id}]? [y/N] ");
            let _ = std::io::stderr().flush();
            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer).is_ok()
                && matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
        })
        .await
        .unwrap_or(false);
        bus.emit(Event::new(ToolEvent::ApprovalResolved {
            call_id,
            approved,
        }));
    }
}
