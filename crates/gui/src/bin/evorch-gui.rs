// allow: SIZE_OK - Existing GUI composition root; T3 only wires UI settings into startup, without restructuring runtime ownership.
use config::agent_categories::CategoryId;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use event_bus::{Event, EventBus, EventKind, LifecycleEvent};
use gui::app::{WorkbenchApp, WorkbenchState};
use gui::diff::FixtureDiffSource;
use gui::events::EventPump;
use gui::model::codex_auth::CodexAuthModel;
use gui::model::codex_auth_backend::{
    DEFAULT_CODEX_CREDENTIAL_ACCOUNT, ProviderCodexAuthBackend, codex_credential_account,
};
use gui::model::composer::{PROVIDER_MISSING_GUIDANCE, ProviderStatus};
use gui::model::demo::DemoScriptModel;
use gui::model::provider_settings::{ProviderSettingsModel, provider_status_of};
use gui::model::telemetry::pricing::SharedUsagePricing;
use gui::pty::ShellSpawner;
use gui::runtime_sink::{
    RuntimeCommandSink, STORAGE_SESSION_ID, derive_base_ref, derive_repo_slug,
};
use gui::storage_bridge::{self, OwnedStorageBridge, StorageBridge};
use routing::ProcessEnv;
use routing::factory::DEFAULT_AUTH_BASE_URL;
use runtime::orchestration::delivery::DeliveryPort;
use runtime::{
    AgentModel, AgentRuntime, ComposedRuntime, CompositionError, ExecutionPolicy,
    FixtureDeliveryAdapter, GoalLedger, GoalSupervisor, ModelSource, OrchestrationSettings, Role,
    RunConfig, RuntimeComposition, ShellDeliveryAdapter, SupervisorHandle, WorkspaceSeam,
    compose_runtime, production_executor_with_config,
};
use sandbox::{BwrapConfig, CredentialError, CredentialStore, Sandbox, Secret, production_sandbox};
use storage::{Database, Storage, StorageConfig};
use workspace_ui::{ProjectId, SidebarState, ThreadId, TrustState, UiSettings};

const EVENT_CAPACITY: usize = 256;

#[derive(Debug, thiserror::Error)]
enum GuiError {
    #[error("argument error: {0}")]
    Arguments(String),
    #[error("settings load failed: {0}")]
    Settings(#[from] workspace_ui::SettingsError),
    #[error("panel keybindings failed: {0}")]
    PanelKeybinds(#[from] gui::keymap::PanelKeybindError),
    #[error("layout load failed: {0}")]
    Layout(#[from] workspace_ui::PersistError),
    #[error("workbench initialization failed: {0}")]
    Workbench(#[from] gui::app::WorkbenchError),
    #[error("GUI initialization failed: {0}")]
    Eframe(String),
    #[error("runtime initialization failed: {0}")]
    Runtime(#[from] runtime::RuntimeError),
    #[error("runtime composition failed: {0}")]
    Composition(#[from] CompositionError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("sidebar state failed: {0}")]
    Sidebar(#[from] workspace_ui::SidebarError),
    #[error("demo project state failed: {0}")]
    Project(#[from] workspace_ui::ProjectError),
    #[error("demo thread state failed: {0}")]
    Thread(#[from] workspace_ui::ThreadError),
    #[error("sidebar state directory initialization failed: {0}")]
    StateDirectory(std::io::Error),
    #[error("storage initialization failed: {0}")]
    Storage(#[from] storage::StorageError),
    #[error("demo repository initialization failed: {0}")]
    DemoRepo(String),
    #[error("delivery sandbox initialization failed: {0}")]
    Sandbox(String),
    #[error("supervisor startup failed: {0}")]
    Supervisor(String),
    #[error("ownership initialization failed: {0}")]
    Ownership(#[from] runtime::ownership::RegistryError),
}

#[derive(Debug, Default)]
struct Arguments {
    demo: bool,
    settings: Option<PathBuf>,
    layout: Option<PathBuf>,
    save_layout: Option<PathBuf>,
    state: Option<PathBuf>,
    window_title: String,
}

fn parse_arguments() -> Result<Arguments, GuiError> {
    parse_arguments_from(std::env::args().skip(1))
}

fn parse_arguments_from(mut values: impl Iterator<Item = String>) -> Result<Arguments, GuiError> {
    let mut arguments = Arguments {
        window_title: String::from("evorch"),
        ..Default::default()
    };
    while let Some(argument) = values.next() {
        match argument.as_str() {
            "--demo" => arguments.demo = true,
            "--settings" => arguments.settings = Some(next_path(&mut values, "--settings")?),
            "--layout" => arguments.layout = Some(next_path(&mut values, "--layout")?),
            "--save-layout" => {
                arguments.save_layout = Some(next_path(&mut values, "--save-layout")?);
            }
            "--state" => arguments.state = Some(next_path(&mut values, "--state")?),
            "--window-title" => {
                arguments.window_title = values.next().ok_or_else(|| {
                    GuiError::Arguments(String::from("--window-title requires a value"))
                })?;
            }
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            unknown => return Err(GuiError::Arguments(format!("unknown option: {unknown}"))),
        }
    }
    Ok(arguments)
}

fn print_help() {
    println!(
        r#"Usage: evorch-gui [--demo] [--settings PATH] [--layout PATH] [--save-layout PATH] [--state PATH] [--window-title <text>]

--window-title <text>  Set the window title (default: evorch).

Demo mode (--demo) runs a deterministic scripted session; no external AI
provider is used or required.

Non-demo mode starts a real AgentRuntime. Goal submission goes through the
entry router and launches a pre-routed background run: an explicit "direct"
keyword starts a Worker run directly; otherwise an Orchestrator run is
started. Delivery (git push / gh pr / intent-cli worker closeout) runs through
ShellDeliveryAdapter, so `gh auth status` must succeed or goals block at the
delivery stage. Only the final merge requires operator approval; the request
appears as a conversation notice, with no approval button in this UI yet.
Every other step (implement, push, PR, CI watch, review, repair,
re-review, closeout) is automatic.
既知の制限: エージェント応答の描画は demo スクリプトモデルのままであり、provider composition root 導入まで実 provider の応答は表示されない。

Sidebar state is loaded from and saved to --state PATH. Without --state, the
default is <user-config-dir>/sidebar.json; if no user config dir is derivable,
sidebar persistence is skipped.

Goal events are persisted to <user-config-dir>/evorch-events.db (demo mode
uses a temporary directory) and Active goals from previous sessions are
adopted as paused on startup.

Start:

    cargo run -p gui --bin evorch-gui -- --demo

Demo mode manual verification:

1. Sidebar: evorch-demo, trusted temp dir, demo-thread-1 (pinned, active), demo-thread-2.
2. Agents: run-1 Orchestrator, run-2 worker-w1, run-3 reviewer-r1 reach Done;
   provider is demo and tokens increase.
3. Click a row to drill down; use ← Thread to return.
4. Open default panes: 3 transcripts; run-2 contains incoming "implement the goal"
   and outgoing "worker done" only.
5. Diff: Working tree shows 2 fixture files; Branch vs main shows 1.
6. Goal loop: send `/goal DEMO-GOAL implement fixture unit` in the conversation
   composer and confirm "accepted: goal-1". The deterministic runtime loop
   implements, reviews, repairs, and then requests merge approval.
7. Merge approval currently appears as a conversation notice only. The runtime
   approval port remains available; a Diff-view approval surface is a follow-up.
   There is no approval button in this UI.
8. Ctrl+S saves layout; Ctrl+Shift+R resets it. `bwrap` must be on PATH.

Requirements:

- `bwrap` must be on PATH. Without it the app prints
  `evorch-gui: runtime initialization failed: サンドボックス構築に失敗しました: ...`
   and exits with code 1."#
    );
}

fn next_path(values: &mut impl Iterator<Item = String>, option: &str) -> Result<PathBuf, GuiError> {
    values
        .next()
        .map(PathBuf::from)
        .ok_or_else(|| GuiError::Arguments(format!("{option} requires a path")))
}

fn load_settings(arguments: &Arguments) -> Result<UiSettings, GuiError> {
    let mut settings = match ui_settings_path(arguments) {
        Some(path) if path.exists() => match workspace_ui::load_settings(&path) {
            Ok(settings) => settings,
            Err(error) => {
                tracing::warn!(%error, "UI settings load failed; using Graphite defaults");
                UiSettings::default()
            }
        },
        Some(_) | None => UiSettings::default(),
    };
    if let Some(layout) = arguments.layout.as_deref() {
        settings.layout.workspace = Some(workspace_ui::load_workspace(layout)?);
    }
    Ok(settings)
}

fn ui_settings_path(arguments: &Arguments) -> Option<PathBuf> {
    arguments
        .settings
        .clone()
        .or_else(|| config::user_config_dir().map(|directory| directory.join("ui.toml")))
}

fn sidebar_path(arguments: &Arguments) -> Option<PathBuf> {
    arguments
        .state
        .clone()
        .or_else(|| config::user_config_dir().map(|directory| directory.join("sidebar.json")))
}

fn load_sidebar(path: Option<&PathBuf>) -> Result<SidebarState, GuiError> {
    let Some(path) = path else {
        return Ok(SidebarState::default());
    };
    if path.exists() {
        return Ok(workspace_ui::load_sidebar(path)?);
    }
    Ok(SidebarState::default())
}

/// The project the GUI was last working in; runs still bind to their thread's project.
fn startup_project(sidebar: &SidebarState) -> Option<&workspace_ui::ProjectRecord> {
    sidebar
        .projects
        .iter()
        .find(|project| Some(&project.id) == sidebar.selected_project.as_ref())
}

fn effective_project_context(
    sidebar: &SidebarState,
    fallback_root: &Path,
) -> (PathBuf, config::LoadOptions) {
    let root = startup_project(sidebar).map_or_else(
        || fallback_root.to_path_buf(),
        |project| project.repo_root.clone(),
    );
    let options = config::LoadOptions {
        project_dir: Some(root.clone()),
        ..Default::default()
    };
    (root, options)
}

fn demo_sidebar(
    repo_root: &std::path::Path,
    allowed_directory: &std::path::Path,
) -> Result<SidebarState, GuiError> {
    let mut sidebar = SidebarState::default();
    let project_id = ProjectId::new("evorch-demo");
    sidebar.add_project(project_id.clone(), "evorch-demo", repo_root)?;
    sidebar.select_project(&project_id)?;
    sidebar.add_allowed_directory(&project_id, allowed_directory, TrustState::Approved)?;
    let active_thread = ThreadId::new("demo-thread-1");
    sidebar.create_thread(active_thread.clone(), project_id.clone(), "demo-thread-1")?;
    sidebar.set_pinned(&active_thread, true)?;
    sidebar.create_thread(ThreadId::new("demo-thread-2"), project_id, "demo-thread-2")?;
    sidebar.switch_thread(&active_thread)?;
    Ok(sidebar)
}

fn demo_diff_source() -> FixtureDiffSource {
    FixtureDiffSource::new(
        Ok("diff --git a/src/demo.rs b/src/demo.rs\n--- a/src/demo.rs\n+++ b/src/demo.rs\n@@ -1 +1 @@\n-old\n+demo\ndiff --git a/tests/demo.rs b/tests/demo.rs\n--- /dev/null\n+++ b/tests/demo.rs\n@@ -0,0 +1 @@\n+demo test\n".to_string()),
        Ok("diff --git a/README.md b/README.md\n--- a/README.md\n+++ b/README.md\n@@ -1 +1 @@\n-old\n+demo branch\n".to_string()),
    )
}

/// --demo 用の隔離 git リポジトリ (1 commit) を作成する。
///
/// demo モードの worker は isolated worktree で動くため、runtime の
/// project root には実在する git リポジトリが必要になる。
fn init_demo_repo(base: &Path) -> Result<PathBuf, GuiError> {
    let repo = base.join("repo");
    std::fs::create_dir_all(&repo)?;
    // Git hooks export repository routing variables; a demo must discover its own repo.
    let local_env = std::process::Command::new("git")
        .args(["rev-parse", "--local-env-vars"])
        .output()?;
    if !local_env.status.success() {
        return Err(GuiError::DemoRepo(format!(
            "git rev-parse --local-env-vars failed: {}",
            String::from_utf8_lossy(&local_env.stderr).trim()
        )));
    }
    let local_env = String::from_utf8_lossy(&local_env.stdout);
    let commands: Vec<(&str, Vec<&str>)> = vec![
        ("git init", vec!["init", "--quiet"]),
        (
            "git config user.email",
            vec!["config", "user.email", "demo@evorch.local"],
        ),
        (
            "git config user.name",
            vec!["config", "user.name", "evorch demo"],
        ),
        (
            "git commit",
            vec![
                "commit",
                "--no-gpg-sign",
                "--allow-empty",
                "--quiet",
                "-m",
                "initial demo commit",
            ],
        ),
    ];
    for (label, args) in commands {
        let mut command = std::process::Command::new("git");
        for variable in local_env.lines() {
            command.env_remove(variable);
        }
        let output = command.args(args).current_dir(&repo).output()?;
        if !output.status.success() {
            return Err(GuiError::DemoRepo(format!(
                "{label} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
    }
    Ok(repo)
}

/// goal イベントの永続化先を決める。
///
/// --demo は `<tempdir>/events.db`、非 demo は
/// `<user-config-dir>/evorch-events.db`。user config dir が導出できない場合は
/// 一時ディレクトリへ fallback し、永続化が再起動を跨がない旨を警告する。
fn storage_db_path(
    demo_directory: Option<&tempfile::TempDir>,
) -> Result<(PathBuf, Option<tempfile::TempDir>), GuiError> {
    if let Some(directory) = demo_directory {
        return Ok((directory.path().join("events.db"), None));
    }
    match config::user_config_dir() {
        Some(directory) => Ok((directory.join("evorch-events.db"), None)),
        None => {
            let fallback = tempfile::tempdir()?;
            tracing::warn!(
                "user config dir is unavailable; goal events fall back to a temporary \
                 directory and will not survive restarts"
            );
            Ok((fallback.path().join("events.db"), Some(fallback)))
        }
    }
}

/// delivery 用の production bwrap sandbox を組立てる (計画 Clarification A)。
///
/// ネットワークを許可し、`gh` / `git` が参照する認証素材
/// (`~/.config/gh`, `~/.gitconfig`) をホストパスのまま読み取り bind する。
/// 存在しない認証素材は bind 元として無効なため除外する。
/// sandbox 内の HOME は /tmp/home に固定されるため、`GH_CONFIG_DIR` /
/// `GIT_CONFIG_GLOBAL` をホストパスでエクスポートしている場合のみ
/// delivery adapter 経由で参照される (credential_env は親環境の転送のみ)。
fn production_delivery_sandbox(repo_root: PathBuf) -> Result<Arc<dyn Sandbox>, GuiError> {
    let mut sandbox_config = BwrapConfig::new(repo_root).allow_network(true);
    for path in credential_ro_binds() {
        sandbox_config = sandbox_config.ro_bind(path);
    }
    production_sandbox(sandbox_config).map_err(|error| GuiError::Sandbox(error.to_string()))
}

/// ホスト側の gh / git 認証素材のうち存在するものを列挙する。
fn credential_ro_binds() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        tracing::warn!(
            "HOME is unset; gh/git credentials are not mounted into the delivery sandbox"
        );
        return Vec::new();
    };
    [home.join(".config").join("gh"), home.join(".gitconfig")]
        .into_iter()
        .filter(|path| path.exists())
        .collect()
}

/// 通常イベントと分単位の usage 集計を保存する bridge を専用 runtime で起動する。
fn spawn_storage_bridge(
    bus: Arc<EventBus>,
    storage: Storage,
    session_id: &'static str,
    diagnostics: config::DiagnosticPersistence,
    metrics_enabled: bool,
    usage_pricing: SharedUsagePricing,
    fault_spool: Option<std::path::PathBuf>,
) -> Result<OwnedStorageBridge, GuiError> {
    let persistence = match diagnostics {
        config::DiagnosticPersistence::Off => storage_bridge::DiagnosticPersistence::Off,
        config::DiagnosticPersistence::Warnings => storage_bridge::DiagnosticPersistence::Warnings,
        config::DiagnosticPersistence::All => storage_bridge::DiagnosticPersistence::All,
    };
    Ok(OwnedStorageBridge::spawn(
        bus,
        storage,
        |handle| {
            let bridge = StorageBridge::new(handle, session_id)
                .with_diagnostic_persistence(persistence)
                .with_metrics_enabled(metrics_enabled)
                .with_usage_ledger(usage_pricing);
            match fault_spool {
                Some(dir) => bridge.with_fault_spool(dir),
                None => bridge,
            }
        },
        Duration::from_secs(60),
    )?)
}

/// Restore each thread's latest objective without restarting work or old authority.
fn latest_thread_goals(
    events: impl IntoIterator<Item = event_bus::Event>,
) -> std::collections::BTreeMap<String, event_bus::ThreadGoalSnapshot> {
    let mut latest = std::collections::BTreeMap::new();
    for event in events {
        if let EventKind::Orchestrator(event_bus::OrchestratorEvent::ThreadGoalUpdated {
            snapshot,
        }) = event.kind
        {
            // One goal can move between threads, and a thread can later
            // receive a different goal. Preserve both uniqueness rules.
            latest.retain(|thread, goal: &mut event_bus::ThreadGoalSnapshot| {
                thread == &snapshot.thread_id || goal.goal_id != snapshot.goal_id
            });
            latest.insert(snapshot.thread_id.clone(), snapshot);
        }
    }
    latest
}

fn restore_thread_goals(storage_config: &StorageConfig, runtime: &runtime::AgentRuntime) {
    let result = Database::open(storage_config).and_then(|database| database.events_all_ordered());
    match result {
        Ok(events) => {
            let latest = latest_thread_goals(events.into_iter().map(|stored| stored.event));
            for snapshot in latest.into_values() {
                if let Err(error) = runtime.restore_thread_goal(snapshot) {
                    tracing::warn!(%error, "failed to restore thread goal");
                }
            }
        }
        Err(error) => tracing::warn!(%error, "failed to read thread goals"),
    }
}

/// Restore procedures for current sidebar conversations without binding old run authority.
fn restore_thread_todos(
    storage_config: &StorageConfig,
    runtime: &runtime::AgentRuntime,
    sidebar: &workspace_ui::SidebarState,
) {
    let result = Database::open(storage_config).and_then(|database| database.events_all_ordered());
    match result {
        Ok(events) => {
            let mut latest = std::collections::BTreeMap::new();
            for stored in events {
                if let EventKind::Orchestrator(event_bus::OrchestratorEvent::ThreadTodoUpdated {
                    snapshot,
                }) = stored.event.kind
                {
                    gui::model::thread_todos::apply_snapshot(&mut latest, &snapshot);
                }
            }
            for snapshot in latest.into_values() {
                if sidebar.threads.iter().any(|thread| {
                    thread.id.to_string() == snapshot.thread_id
                        && sidebar
                            .projects
                            .iter()
                            .any(|project| project.id == thread.project_id)
                }) && let Err(error) = runtime.restore_thread_todo(snapshot)
                {
                    tracing::warn!(%error, "failed to restore thread procedures");
                }
            }
        }
        Err(error) => tracing::warn!(%error, "failed to read thread procedures"),
    }
}

/// 前セッションの PR goal 状態を永続化イベントから復元し、supervisor へ移管する。
///
/// `Database::events_all_ordered()` → `GoalLedger::replay_partial` で goal ごとの
/// snapshot を再構築し、transcript を `agent_messages_by_session` で付与して
/// `adopt` する。永続データは外部入力なので解決不能なイベントは警告して読み飛
/// ばす。Active goal は supervisor 側で Paused
/// (`recovered-after-restart`) として採用される (計画 Clarification B)。
fn restore_goals(storage_config: &StorageConfig, supervisor: &SupervisorHandle) {
    let database = match Database::open(storage_config) {
        Ok(database) => database,
        Err(error) => {
            tracing::warn!(%error, "failed to open events database for goal restore");
            return;
        }
    };
    let events = match database.events_all_ordered() {
        Ok(events) => events,
        Err(error) => {
            tracing::warn!(%error, "failed to read persisted events for goal restore");
            return;
        }
    };
    let orchestrator_events = events.iter().filter_map(|stored| match &stored.event.kind {
        EventKind::Orchestrator(event) => Some(event),
        _ => None,
    });
    let (goals_map, replay_errors) = GoalLedger::replay_partial(orchestrator_events);
    if !replay_errors.is_empty() {
        tracing::warn!(
            count = replay_errors.len(),
            "goal replay skipped unresolvable durable events"
        );
    }
    let goals = goals_map
        .into_values()
        .map(|ledger| {
            let snapshot = ledger.snapshot().clone();
            let transcript = match database.agent_messages_by_session(&snapshot.session_id) {
                Ok(messages) => messages.into_iter().map(|stored| stored.message).collect(),
                Err(error) => {
                    tracing::warn!(
                        %error,
                        goal_id = %snapshot.goal_id,
                        "failed to restore agent transcript"
                    );
                    Vec::new()
                }
            };
            (snapshot, transcript)
        })
        .collect::<Vec<_>>();
    if goals.is_empty() {
        return;
    }
    if let Err(error) = supervisor.adopt(goals) {
        tracing::warn!(%error, "failed to adopt persisted goals");
    }
}

/// 設定読み込みを best-effort で行い、orchestration 設定のみ抽出する
/// (計画 Clarification C)。
///
/// GUI binary は config::Config を必須としないため、読み込み失敗時は
/// 既定値へ fallback して警告を出す。
fn orchestration_settings_or_default(
    loaded: &Result<config::Config, config::ConfigError>,
) -> OrchestrationSettings {
    match loaded {
        Ok(config) => OrchestrationSettings::from(&config.orchestration),
        Err(error) => {
            tracing::warn!(%error, "config load failed; using default orchestration settings");
            OrchestrationSettings::default()
        }
    }
}

fn detected_provider_status(
    loaded: &Result<config::Config, config::ConfigError>,
) -> ProviderStatus {
    match loaded {
        Ok(config) => provider_status_of(config),
        Err(_) => {
            tracing::warn!("config load failed; provider remains not configured");
            ProviderStatus::NotConfigured {
                guidance: PROVIDER_MISSING_GUIDANCE.to_owned(),
            }
        }
    }
}

fn spawn_event_bridge(
    bus: Arc<EventBus>,
    repaint: Option<Arc<dyn Fn() + Send + Sync>>,
) -> Result<(EventPump, tokio::runtime::Handle), GuiError> {
    let (pump_sender, pump_receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name(String::from("evorch-event-pump"))
        .spawn(move || {
            // supervisor の stall tick (tokio::time::interval) と demo model の
            // gate (tokio::time::sleep/timeout) がこの runtime 上で動くため、
            // time driver が必要。
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    tracing::error!(%error, "failed to create tokio runtime");
                    return;
                }
            };
            let pump = EventPump::spawn(&runtime.handle().clone(), bus.subscribe(), repaint);
            if pump_sender.send((pump, runtime.handle().clone())).is_err() {
                return;
            }
            runtime.block_on(std::future::pending::<()>());
        })
        .map_err(|error| GuiError::Arguments(format!("event bridge thread failed: {error}")))?;
    pump_receiver
        .recv()
        .map_err(|error| GuiError::Arguments(format!("event bridge startup failed: {error}")))
}

/// ModelSource::Fixed の runtime は credential store を消費しないため、この
/// ストアは構築副作用を持たない。もし将来 Configured 経路へ切り替えた場合に
/// 誤って参照されても fail-closed でエラーになる。
struct UnwiredCredentialStore;

impl CredentialStore for UnwiredCredentialStore {
    fn get(&self, _key: &str) -> Result<Option<Secret>, CredentialError> {
        Err(Self::unwired())
    }

    fn set(&self, _key: &str, _value: &Secret) -> Result<(), CredentialError> {
        Err(Self::unwired())
    }

    fn delete(&self, _key: &str) -> Result<(), CredentialError> {
        Err(Self::unwired())
    }
}

impl UnwiredCredentialStore {
    fn unwired() -> CredentialError {
        CredentialError::KeychainUnavailable {
            detail: String::from("GUI runtime は credential store を接続していない"),
        }
    }
}

fn credential_dir(demo_directory: Option<&tempfile::TempDir>) -> Option<PathBuf> {
    match demo_directory {
        Some(directory) => Some(directory.path().join("credentials")),
        None => config::user_config_dir().map(|directory| directory.join("credentials")),
    }
}

fn codex_auth_model(
    loaded: Option<&Result<config::Config, config::ConfigError>>,
    credential_store: Option<Arc<dyn CredentialStore>>,
) -> CodexAuthModel {
    let account = loaded
        .and_then(|loaded| loaded.as_ref().ok())
        .and_then(codex_credential_account)
        .unwrap_or_else(|| DEFAULT_CODEX_CREDENTIAL_ACCOUNT.to_owned());
    let Some(store) = credential_store else {
        tracing::warn!("credential directory unavailable; Codex login remains disabled");
        return CodexAuthModel::default();
    };
    match ProviderCodexAuthBackend::production(store, account.clone(), DEFAULT_AUTH_BASE_URL) {
        Ok(backend) => CodexAuthModel::with_backend(Arc::new(backend), account),
        Err(error) => {
            tracing::warn!(%error, "Codex backend initialization failed; login remains disabled");
            CodexAuthModel::default()
        }
    }
}

fn run() -> Result<(), GuiError> {
    gui::logging::init();
    // Panicking runs then report where they panicked; the crash spool wraps this later.
    runtime::panic_capture::install();
    let arguments = parse_arguments()?;
    let mut settings = load_settings(&arguments)?;
    let repo_root = std::fs::canonicalize(std::env::current_dir()?)?;
    let state_path = sidebar_path(&arguments);
    if let Some(parent) = state_path.as_deref().and_then(std::path::Path::parent) {
        std::fs::create_dir_all(parent).map_err(GuiError::StateDirectory)?;
    }
    let demo_directory = arguments.demo.then(tempfile::tempdir).transpose()?;
    let sidebar = match demo_directory.as_ref() {
        Some(directory) => demo_sidebar(&repo_root, directory.path())?,
        None => load_sidebar(state_path.as_ref())?,
    };
    let (effective_project_root, load_options) = effective_project_context(&sidebar, &repo_root);
    let loaded_config: Option<Result<config::Config, config::ConfigError>> = if arguments.demo {
        None
    } else {
        Some(config::Config::load(&load_options))
    };
    let bus = Arc::new(EventBus::new(EVENT_CAPACITY));

    let settings_store: Option<Arc<dyn CredentialStore>> = credential_dir(demo_directory.as_ref())
        .and_then(
            |directory| match sandbox::credential::open_default(directory) {
                Ok(store) => {
                    let store: Arc<dyn CredentialStore> = store;
                    Some(store)
                }
                Err(error) => {
                    tracing::error!(%error, "credential store initialization failed");
                    None
                }
            },
        );
    let composition_config = match loaded_config.as_ref() {
        Some(Ok(config)) => config.clone(),
        Some(Err(error)) => {
            tracing::error!(%error, "provider configuration load failed");
            config::Config::default()
        }
        None => config::Config::default(),
    };
    settings.keybinds = gui::keymap::panel_keybinds(&settings.keybinds, &composition_config.panel)?;
    // Retain the fail-closed store when the credential directory cannot be opened.
    let credential_store: Arc<dyn CredentialStore> = settings_store
        .clone()
        .unwrap_or_else(|| Arc::new(UnwiredCredentialStore));
    let production_model = (!arguments.demo).then(|| {
        let context = gui::model::production::ProductionModel {
            load_options: load_options.clone(),
            credential_store: credential_store.clone(),
            bus: bus.clone(),
            env: Arc::new(ProcessEnv),
        };
        let initial: Arc<dyn AgentModel> = if settings_store.is_some() {
            match gui::model::production::compose_production_model(&composition_config, &context) {
                Ok(model) => model,
                Err(error) => {
                    tracing::error!(%error, "provider model composition failed");
                    Arc::new(runtime::compose::UnconfiguredModel)
                }
            }
        } else {
            Arc::new(runtime::compose::UnconfiguredModel)
        };
        (
            context,
            Arc::new(runtime::compose::SwitchableModel::new(initial)),
        )
    });
    let runtime = match demo_directory.as_ref() {
        Some(directory) => {
            let demo_repo = init_demo_repo(directory.path())?;
            let seam = WorkspaceSeam::production(demo_repo.clone())?;
            let executor = production_executor_with_config(
                Arc::clone(&bus),
                &ExecutionPolicy::for_role(Role::Orchestrator),
                seam.repo_root().to_path_buf(),
                &composition_config,
            )?;
            let demo_model: Arc<dyn AgentModel> =
                Arc::new(DemoScriptModel::new(Arc::clone(&bus)).with_workspace_root(demo_repo));
            let ComposedRuntime {
                runtime,
                model_identity,
            } = compose_runtime(RuntimeComposition {
                user_config_dir: Some(directory.path().to_path_buf()),
                config: &composition_config,
                bus: Arc::clone(&bus),
                executor,
                credential_store,
                env: Arc::new(ProcessEnv),
                model_source: ModelSource::Fixed(demo_model),
                workspace: Some(seam),
            })?;
            tracing::debug!(
                ?model_identity,
                "demo runtime composed via provider composition root"
            );
            runtime
        }
        None => {
            let seam = WorkspaceSeam::production(effective_project_root.clone())?;
            let executor = production_executor_with_config(
                Arc::clone(&bus),
                &ExecutionPolicy::for_role(Role::Orchestrator),
                effective_project_root.clone(),
                &composition_config,
            )?;
            let ComposedRuntime {
                runtime,
                model_identity,
            } = compose_runtime(RuntimeComposition {
                user_config_dir: None,
                config: &composition_config,
                bus: Arc::clone(&bus),
                executor,
                credential_store,
                env: Arc::new(ProcessEnv),
                model_source: ModelSource::Fixed(match &production_model {
                    Some((_, model)) => model.clone(),
                    None => Arc::new(runtime::compose::UnconfiguredModel),
                }),
                workspace: Some(seam),
            })?;
            tracing::debug!(
                ?model_identity,
                "runtime composed via provider composition root"
            );
            runtime
        }
    };

    // Runs of other projects use models composed from those projects' role profiles.
    let project_models = production_model
        .as_ref()
        .map(|(context, _)| gui::model::production::ProjectModels::new(context.clone()));
    let runtime = match &project_models {
        Some(models) => runtime.with_project_models(models.resolver()),
        None => runtime,
    };
    let runtime = if arguments.demo {
        runtime
    } else {
        runtime.with_sandbox_root(effective_project_root.clone())
    };
    let sandbox_runtime = runtime.clone();
    let (storage_db_path, storage_fallback) = storage_db_path(demo_directory.as_ref())?;
    let snapshot_directory = tempfile::tempdir()?;
    let snapshot_root = match demo_directory.as_ref() {
        Some(directory) => directory.path().join("repo"),
        None => effective_project_root.clone(),
    };
    let runtime = runtime.with_snapshots(Arc::new(
        runtime::snapshot::SnapshotService::new(&snapshot_root, snapshot_directory.path())
            .map_err(|error| GuiError::Arguments(error.to_string()))?,
    ));
    let storage_config = StorageConfig {
        db_path: storage_db_path,
        usage_retention_days: composition_config.metrics.retention_days,
        ..StorageConfig::default()
    };
    let storage = Storage::open(storage_config.clone())?;
    let runtime = match runtime::RunStore::open(&storage_config, storage.handle()) {
        Ok(store) => runtime.with_run_store(store),
        Err(error) => {
            tracing::warn!(%error, "run store unavailable; continuing without persistent run restore");
            runtime
        }
    };
    let quick_route = composition_config
        .agents
        .binding_for("worker", Some(CategoryId::Quick.as_str()))
        .ok()
        .and_then(|binding| {
            composition_config
                .routing
                .routes
                .get(&binding.logical_model)
        })
        .and_then(|routes| routes.first());
    // Each run's lessons are partitioned by the project it worked in.
    let runtime = runtime.with_project_slugs(Arc::new(derive_repo_slug));
    let runtime = match quick_route {
        Some(route) => runtime.with_learning(runtime::memory_queue::LearningSettings {
            writer: storage.handle(),
            storage: storage_config.clone(),
            project: derive_repo_slug(&repo_root),
            quick: runtime::ModelPreference {
                profile: route.profile.clone(),
                model: route.model.clone(),
                reasoning_effort: route.reasoning_effort.clone(),
            },
        }),
        None => {
            tracing::warn!("post-run learning unavailable: configure a worker quick model route");
            runtime
        }
    };

    let repaint_ctx = Arc::new(OnceLock::<egui::Context>::new());
    let repaint_hook = {
        let context_slot = Arc::clone(&repaint_ctx);
        Arc::new(move || {
            if let Some(context) = context_slot.get() {
                context.request_repaint();
            }
        })
    };
    let (pump, handle) = spawn_event_bridge(Arc::clone(&bus), Some(repaint_hook))?;

    let self_improvement = &composition_config.self_improvement;
    // Keep the disabled path inert: no path creation, collector, or panic hook.
    let improvement_draft_dir = self_improvement.enabled.then(|| {
        gui::model::self_improvement_settings::resolve_draft_dir(
            self_improvement,
            &storage_config.db_path,
        )
    });
    let runtime = if let Some(drafts_dir) = &improvement_draft_dir {
        let policy = runtime::self_improvement::ImprovementPolicy {
            draft_dir: Some(drafts_dir.clone()),
            evidence_max_bytes: self_improvement.evidence_max_bytes,
            daily_limit: self_improvement.daily_limit,
            duplicate_cooldown_secs: self_improvement.duplicate_cooldown_secs,
            max_candidates: self_improvement.max_candidates,
            collect_diagnostics: self_improvement.collect_diagnostics,
            collect_lessons: self_improvement.collect_lessons,
        };
        let runtime =
            runtime.with_self_improvement(runtime::self_improvement::ImprovementSettings {
                writer: storage.handle(),
                project: derive_repo_slug(&repo_root),
                policy,
            });
        // The GUI thread is synchronous; enter the existing event-pump runtime
        // so the observer's tokio::spawn runs on its continuously driven executor.
        {
            let _guard = handle.enter();
            runtime.start_self_improvement();
        }
        runtime.ingest_spooled_crashes();
        // The spool lives inside the resolved drafts directory, matching runtime intake.
        let spool_dir = drafts_dir.join("crash-spool");
        let directories =
            std::fs::create_dir_all(drafts_dir).and_then(|()| std::fs::create_dir_all(&spool_dir));
        match directories {
            Ok(()) => runtime::self_improvement::install_crash_spool(spool_dir),
            Err(error) => {
                tracing::warn!(%error, "self-improvement crash spool unavailable; skipping panic hook");
            }
        }
        runtime
    } else {
        runtime
    };

    // --demo は常に既定値を使い、非 demo のみ config 読み込みを試みる
    // (計画 Clarification C)。
    let (orchestration, mut provider_status, provider_settings) = match loaded_config.as_ref() {
        Some(loaded) => (
            orchestration_settings_or_default(loaded),
            detected_provider_status(loaded),
            match loaded {
                Ok(config) => {
                    let mut settings = ProviderSettingsModel::seed_from_config(config);
                    settings
                        .catalog
                        .start(gui::model::model_catalog::CatalogRequest::Load);
                    settings
                }
                Err(_) => ProviderSettingsModel::default(),
            },
        ),
        None => (
            OrchestrationSettings::default(),
            ProviderStatus::NotConfigured {
                guidance: PROVIDER_MISSING_GUIDANCE.to_owned(),
            },
            ProviderSettingsModel::default(),
        ),
    };
    if production_model.as_ref().is_some_and(|(_, model)| {
        model
            .selected_model(Role::Worker, None)
            .starts_with("unresolved:")
    }) {
        provider_status = ProviderStatus::NotConfigured {
            guidance: PROVIDER_MISSING_GUIDANCE.to_owned(),
        };
    }
    // The demo keeps its settings in an isolated user layer: projects may only select a role profile.
    let provider_settings_path = match demo_directory.as_ref() {
        Some(directory) => directory.path().join("config.toml"),
        None => config::user_main_config_path().unwrap_or_else(|| {
            tracing::warn!(
                "user config directory unavailable; settings saved to the project config are ignored on load (projects may only select a role profile)"
            );
            config::project_main_config_path(&effective_project_root)
        }),
    };
    // Config writers require an existing parent (notably the demo's new .evorch directory).
    if let Some(parent) = provider_settings_path.parent() {
        std::fs::create_dir_all(parent).map_err(GuiError::StateDirectory)?;
    }
    let settings_load_options = config::LoadOptions {
        project_dir: Some(demo_directory.as_ref().map_or_else(
            || effective_project_root.clone(),
            |directory| directory.path().to_path_buf(),
        )),
        user_config_dir: demo_directory
            .as_ref()
            .map(|directory| directory.path().to_path_buf()),
        read_env: false,
        ..Default::default()
    };

    let delivery: Arc<dyn DeliveryPort> = match demo_directory.as_ref() {
        Some(_) => Arc::new(FixtureDeliveryAdapter::scripted_happy_path()),
        None => {
            let sandbox = production_delivery_sandbox(repo_root.clone())?;
            Arc::new(ShellDeliveryAdapter::new(
                Arc::clone(&bus),
                sandbox,
                repo_root.clone(),
                derive_repo_slug(&repo_root),
                derive_base_ref(&repo_root),
            ))
        }
    };

    // supervisor actor は pump runtime 上で動くため、runtime context 内で
    // 生成して handle を返してもらう。
    let (supervisor_tx, supervisor_rx) = std::sync::mpsc::channel();
    {
        let runtime = runtime.clone();
        let bus = Arc::clone(&bus);
        handle.spawn(async move {
            let supervisor = GoalSupervisor::spawn(runtime, bus, delivery, orchestration);
            let _ = supervisor_tx.send(supervisor);
        });
    }
    let supervisor = supervisor_rx
        .recv()
        .map_err(|error| GuiError::Supervisor(format!("supervisor task ended: {error}")))?;

    // From this point onward every early-return path joins the bridge before
    // closing SQLite, even while runtime owners still hold the event bus.
    // Seeded from static profile prices; the GUI adds the model catalog once loaded.
    let usage_pricing: SharedUsagePricing = Arc::new(std::sync::RwLock::new(
        gui::model::telemetry::pricing::UsagePricing::new(
            composition_config.providers.clone(),
            None,
        ),
    ));
    let storage = spawn_storage_bridge(
        Arc::clone(&bus),
        storage,
        STORAGE_SESSION_ID,
        composition_config.diagnostics.persistence,
        composition_config.metrics.enabled,
        Arc::clone(&usage_pricing),
        // The crash spool the runtime drains at startup (see the panic hook above).
        improvement_draft_dir
            .as_ref()
            .map(|drafts| drafts.join("crash-spool")),
    )?;
    restore_goals(&storage_config, &supervisor);
    restore_thread_goals(&storage_config, &runtime);
    restore_thread_todos(&storage_config, &runtime, &sidebar);

    let ownership_root = match demo_directory.as_ref() {
        Some(directory) => directory.path().join("threads"),
        None => std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::home_dir().map(|home| home.join(".local/state")))
            .ok_or_else(|| GuiError::Arguments("No state directory for ownership".into()))?
            .join("evorch/threads"),
    };
    let ownership_settings = loaded_config
        .as_ref()
        .and_then(|loaded| loaded.as_ref().ok())
        .map(|config| config.ownership.clone())
        .unwrap_or_default();
    let ownership = Arc::new(runtime::ownership::OwnerHost::open(
        &ownership_root,
        ownership_settings,
        Arc::clone(&bus),
    )?);
    // Install the retained host before any later fallible startup work. State
    // and command-sink locals may drop first, but cannot release this generation.
    let storage = GuiStorageResources {
        _storage: storage,
        ownership: Arc::clone(&ownership),
        _demo_directory: demo_directory,
        _storage_fallback: storage_fallback,
    };
    // goal 投入から run 起動・supervisor 登録・merge/pause/resume/cancel までを
    // production 経路で接続する CommandSink (demo も同様)。
    let mut state = WorkbenchState::new(runtime.clone(), &settings)?
        .with_folder_picker(Arc::new(gui::model::folder_picker::PortalFolderPicker))
        .with_provider_status(provider_status)
        .with_provider_settings(provider_settings)
        .with_settings_load_options(settings_load_options)
        .with_provider_settings_path(provider_settings_path)
        .with_sandbox_runtime(sandbox_runtime)
        .with_codex_auth(codex_auth_model(
            loaded_config.as_ref(),
            settings_store.clone(),
        ))
        .with_pump(pump)
        .with_terminal_spawner(Arc::new(ShellSpawner::from_env()))
        .with_ownership(Arc::clone(&ownership))
        .with_command_sink(Box::new(
            RuntimeCommandSink::new(runtime.clone(), handle.clone(), supervisor)
                .with_web_tools_enabled(composition_config.sandbox.web_tools_enabled)
                .with_event_bus(Arc::clone(&bus))
                .with_ownership(ownership)
                .with_memory_storage(storage_config.clone())
                .with_team_writer(storage.handle()),
        ));
    match gui::model::system_notifications::NativeSystemNotificationSink::new() {
        Ok(sink) => state = state.with_system_notifications(Arc::new(sink)),
        Err(error) => tracing::warn!(%error, "system notification worker unavailable"),
    }
    if let Some(path) = ui_settings_path(&arguments) {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).map_err(GuiError::StateDirectory)?;
        }
        state = state.with_ui_settings_path(path);
    }
    if let Some(store) = settings_store {
        state = state.with_credential_store(store);
        if let Some((context, model)) = production_model {
            state = state.with_production_model(context, model);
        }
        if let Some(models) = project_models {
            state = state.with_project_models(models);
        }
    }
    state = state.with_sidebar(sidebar);
    if let Some(executable) = std::env::var_os("EVORCH_SLASH_COMMAND_EXECUTABLE") {
        state.load_external_commands(executable.into());
    }
    if let Some(path) = state_path {
        state = state.with_sidebar_path(path);
    }
    if let Some(path) = arguments.save_layout {
        state = state.with_save_path(path);
    }
    state = state
        .with_memory_storage(storage_config.clone())
        .with_diagnostic_storage(storage.handle(), &storage_config)
        .with_self_improvement(
            storage.handle(),
            self_improvement.enabled,
            improvement_draft_dir,
        )
        .with_usage_ledger(storage.handle(), usage_pricing);
    state.restore_history(&storage::Database::open(&storage_config)?)?;

    if arguments.demo {
        state = state.with_diff_source(Arc::new(demo_diff_source()));
        bus.emit(Event::new(LifecycleEvent::Started {
            session_id: String::from("gui-demo"),
        }));
        // demo 起動 run も entry pre-routing 経由で role を決定する。
        // "DEMO-ORCH" に direct キーワードは無いため Coordinated → Orchestrator となり、
        // 従来の固定 Orchestrator 起動と同一の挙動。
        let demo_runtime = runtime.clone();
        handle.spawn(async move {
            let decision = demo_runtime.entry_router().classify("DEMO-ORCH").await;
            demo_runtime.delegate_background(
                decision.role(),
                String::from("DEMO-ORCH"),
                RunConfig::default(),
            );
        });
    }

    // One captured owner preserves shutdown ordering even if run_native fails
    // before invoking its app constructor and only drops the closure captures.
    let storage_resources = storage;
    let title = arguments.window_title;
    let options = gui::window::native_options(&title);
    eframe::run_native(
        &title,
        options,
        Box::new(move |creation_context| {
            let _ = repaint_ctx.set(creation_context.egui_ctx.clone());
            state.reload_theme(&creation_context.egui_ctx, settings.theme_preset.into());
            Ok(Box::new(GuiApp {
                workbench: WorkbenchApp(state),
                #[cfg(feature = "browser")]
                browser: gui::browser::BrowserWindow::new(bus.clone(), handle.clone()),
                _storage: storage_resources,
            }))
        }),
    )
    .map_err(|error| GuiError::Eframe(error.to_string()))
}

struct GuiApp {
    workbench: WorkbenchApp<AgentRuntime>,
    #[cfg(feature = "browser")]
    browser: gui::browser::BrowserWindow,
    _storage: GuiStorageResources,
}

// Retain the host across all other GUI/state drops. Its Drop releases owners,
// which must happen only after queued generation-guarded events are durable.
struct GuiStorageResources {
    _storage: OwnedStorageBridge,
    ownership: Arc<runtime::ownership::OwnerHost>,
    _demo_directory: Option<tempfile::TempDir>,
    _storage_fallback: Option<tempfile::TempDir>,
}

impl GuiStorageResources {
    fn handle(&self) -> storage::StorageHandle {
        self._storage.handle()
    }
}

impl Drop for GuiStorageResources {
    fn drop(&mut self) {
        if let Err(error) = self.ownership.begin_quiesce() {
            tracing::warn!(%error, "failed to quiesce ownership before storage drain");
        }
        if let Err(error) = self._storage.flush() {
            tracing::warn!(%error, "failed to flush queued events before ownership release");
        }
        if let Err(error) = self.ownership.release_ready() {
            tracing::warn!(%error, "failed to release ownership after storage drain");
        }
        // The bridge is still subscribed when release_ready emits Released.
        // Its final snapshot durably records that transition after prior deltas.
        self._storage.shutdown();
        // Fields then close SQLite, drop the retained host and remove temp dirs.
    }
}

impl eframe::App for GuiApp {
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        self.workbench.raw_input_hook(ctx, raw_input);
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        #[cfg(feature = "browser")]
        self.browser.render(ui);
        self.workbench.0.ui(ui, frame);
    }
}

impl Drop for GuiApp {
    fn drop(&mut self) {
        self.workbench.0.save_sidebar();
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("evorch-gui: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::orchestration_settings_or_default;
    use config::ConfigError;
    use runtime::OrchestrationSettings;

    #[test]
    fn restoring_goals_keeps_latest_thread_objective_and_one_handoff_owner() {
        let update = |goal: &str, thread: &str, run: &str| {
            event_bus::Event::new(event_bus::OrchestratorEvent::ThreadGoalUpdated {
                snapshot: event_bus::ThreadGoalSnapshot {
                    goal_id: goal.into(),
                    thread_id: thread.into(),
                    root_run_id: run.into(),
                    related_root_run_ids: vec![],
                    objective: "Verify the implementation".into(),
                    original_request: "Implement".into(),
                    criteria: vec![],
                    checks: vec![],
                    phase: event_bus::ThreadGoalPhase::Working,
                    review_enabled: false,
                    checks_paused: false,
                    work_stopped: false,
                    epoch: 1,
                    review_round: 0,
                    findings: vec![],
                    reason: None,
                    usage: event_bus::ThreadGoalUsage::default(),
                    max_review_rounds: 3,
                    max_tokens: None,
                },
            })
        };
        let mut events = vec![
            update("goal-z", "worker", "run-1"),
            update("goal-z", "escalation-run-2", "run-2"),
        ];
        let inherited = super::latest_thread_goals(events.clone());
        assert_eq!(inherited.len(), 1);
        assert!(!inherited.contains_key("worker"));
        assert_eq!(inherited["escalation-run-2"].goal_id, "goal-z");
        events.extend([
            // A subsequent goal replaces the completed inherited objective.
            update("goal-a", "escalation-run-2", "run-4"),
            update("goal-new-worker", "worker", "run-5"),
        ]);
        let latest = super::latest_thread_goals(events);
        assert_eq!(latest.len(), 2);
        assert_eq!(latest["escalation-run-2"].goal_id, "goal-a");
        assert_eq!(latest["escalation-run-2"].root_run_id, "run-4");
        assert_eq!(latest["worker"].goal_id, "goal-new-worker");
    }

    #[test]
    fn demo_repo_isolates_inherited_git_repository() {
        const CHILD_BASE: &str = "EVORCH_DEMO_REPO_ISOLATION_TEST";
        if let Some(base) = std::env::var_os(CHILD_BASE) {
            let repo = super::init_demo_repo(std::path::Path::new(&base)).unwrap();
            assert!(repo.join(".git").is_dir(), "demo owns its Git repository");
            return;
        }

        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        std::fs::create_dir(&source).unwrap();
        assert!(
            std::process::Command::new("git")
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap())
                .args(["init", "--quiet"])
                .current_dir(&source)
                .status()
                .unwrap()
                .success()
        );
        let git_dir = source.join(".git");
        let config_path = git_dir.join("config");
        let config = format!(
            "{}\n[user]\n\tname = Existing User\n\temail = existing@example.com\n",
            std::fs::read_to_string(&config_path).unwrap()
        );
        std::fs::write(&config_path, &config).unwrap();

        // A subprocess models hook inheritance without mutating this test runner's env.
        assert!(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tests::demo_repo_isolates_inherited_git_repository",
                    "--nocapture",
                ])
                .env(CHILD_BASE, temp.path().join("demo"))
                .env("GIT_DIR", &git_dir)
                .env("GIT_COMMON_DIR", &git_dir)
                .env("GIT_WORK_TREE", &source)
                .env("GIT_INDEX_FILE", git_dir.join("index"))
                .status()
                .unwrap()
                .success()
        );
        assert_eq!(std::fs::read_to_string(config_path).unwrap(), config);
        assert_eq!(
            std::fs::read_dir(git_dir.join("refs/heads"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn startup_restores_theme_from_explicit_settings() {
        // Given: a persisted preset in an isolated explicit settings path.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ui.toml");
        let settings = workspace_ui::UiSettings {
            theme_preset: workspace_ui::ThemePresetName::TokyoNight,
            ..Default::default()
        };
        workspace_ui::save_settings(&settings, &path).unwrap();
        let arguments = super::Arguments {
            settings: Some(path),
            ..Default::default()
        };
        // When: the production startup loader runs.
        let loaded = super::load_settings(&arguments).unwrap();
        // Then: the persisted preset reaches startup unchanged.
        assert_eq!(
            loaded.theme_preset,
            workspace_ui::ThemePresetName::TokyoNight
        );
    }

    #[test]
    fn startup_uses_graphite_when_settings_are_invalid() {
        // Given: an invalid settings file in an isolated directory.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ui.toml");
        std::fs::write(&path, "not valid TOML [").unwrap();
        let arguments = super::Arguments {
            settings: Some(path),
            ..Default::default()
        };
        // When: the production startup loader cannot parse it.
        let loaded = super::load_settings(&arguments).unwrap();
        // Then: startup can continue with Graphite.
        assert_eq!(loaded.theme_preset, workspace_ui::ThemePresetName::Graphite);
    }

    #[test]
    fn window_title_is_selected_when_provided() {
        // Given: a unique title for a smoke-test window.
        let values = ["--window-title", "evorch-smoke-1"].map(String::from);
        // When: the arguments are parsed without starting the GUI.
        let arguments = super::parse_arguments_from(values.into_iter()).expect("valid arguments");
        // Then: the requested title is retained.
        assert_eq!(arguments.window_title, "evorch-smoke-1");
    }

    #[test]
    fn window_title_requires_value_when_missing() {
        // Given: the title option has no following value.
        let values = [String::from("--window-title")];
        // When: the arguments are parsed.
        let error = super::parse_arguments_from(values.into_iter()).expect_err("missing value");
        // Then: the existing argument-error variant explains the missing value.
        assert!(matches!(error, super::GuiError::Arguments(message)
            if message == "--window-title requires a value"));
    }

    #[test]
    fn window_title_defaults_when_omitted() {
        // Given: no command-line options.
        let values = std::iter::empty();
        // When: the arguments are parsed.
        let arguments = super::parse_arguments_from(values).expect("default arguments");
        // Then: normal launches retain the existing title.
        assert_eq!(arguments.window_title, "evorch");
    }

    #[test]
    fn codex_auth_model_has_no_backend_without_credential_dir() {
        // Given: no credential directory is available.
        // When: the login model is constructed.
        let model = super::codex_auth_model(None, None);
        // Then: login remains unavailable without preventing startup.
        assert!(!model.has_backend());
        assert_eq!(
            model.state,
            gui::model::codex_auth::CodexAuthState::Unauthenticated
        );
    }

    #[test]
    fn codex_auth_model_uses_profile_account_and_opens_store_in_temp_dir() {
        // Given: a Codex profile overrides the default account.
        let mut config = config::Config::default();
        config.providers.insert(
            "c".into(),
            config::ProviderProfileConfig {
                provider_type: config::ProviderTypeConfig::OpenAiCodex,
                credential: config::CredentialRefConfig::Keyring {
                    service: "evorch".into(),
                    account: "work".into(),
                },
                ..Default::default()
            },
        );
        for (config, account) in [(config, "work"), (config::Config::default(), "codex")] {
            let directory = tempfile::tempdir().expect("credential directory");
            // When: the production backend is constructed without starting login.
            let model = super::codex_auth_model(
                Some(&Ok(config)),
                Some(std::sync::Arc::new(
                    sandbox::FileCredentialStore::open(directory.path()).unwrap(),
                )),
            );
            // Then: the backend uses the selected account.
            assert!(model.has_backend());
            assert_eq!(model.credential_account, account);
        }
    }

    #[test]
    fn detected_provider_status_fails_closed_on_config_error() {
        // Given: config loading failed
        let loaded = Err(ConfigError::Migration("test".to_owned()));
        // When: provider availability is detected
        let status = super::detected_provider_status(&loaded);
        // Then: submission remains disabled with the existing guidance
        assert_eq!(
            status,
            super::ProviderStatus::NotConfigured {
                guidance: super::PROVIDER_MISSING_GUIDANCE.to_owned(),
            }
        );
    }

    #[test]
    fn detected_provider_status_is_configured_with_one_provider() {
        // Given: one registered provider
        let mut config = config::Config::default();
        config
            .providers
            .insert("local".into(), config::ProviderProfileConfig::default());
        // When: provider availability is detected
        let status = super::detected_provider_status(&Ok(config));
        // Then: the provider is configured
        assert_eq!(status, super::ProviderStatus::Configured);
    }

    #[test]
    fn detected_provider_status_is_not_configured_without_providers() {
        // Given: an empty provider configuration
        let loaded = Ok(config::Config::default());
        // When: provider availability is detected
        let status = super::detected_provider_status(&loaded);
        // Then: submission remains disabled
        assert_eq!(
            status,
            super::ProviderStatus::NotConfigured {
                guidance: super::PROVIDER_MISSING_GUIDANCE.to_owned(),
            }
        );
    }

    // Given: config 読み込みが失敗したとき
    // When: orchestration 設定を解決する
    // Then: 既定値へ fallback する
    #[test]
    fn orchestration_settings_fall_back_to_default_on_config_error() {
        assert_eq!(
            orchestration_settings_or_default(&Err(ConfigError::Migration("test".to_owned()))),
            OrchestrationSettings::default()
        );
    }
}

#[cfg(test)]
mod effective_project_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingSandbox(AtomicUsize);

    impl sandbox::Sandbox for CountingSandbox {
        fn wrap(
            &self,
            spec: sandbox::CommandSpec,
        ) -> Result<sandbox::WrappedCommand, sandbox::SandboxError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            sandbox::DirectSandbox::new_unchecked().wrap(spec)
        }
    }

    struct CountingFactory(Arc<CountingSandbox>);

    impl runtime::SandboxFactory for CountingFactory {
        fn build(
            &self,
            _: &ExecutionPolicy,
            _: &runtime::IsolatedMounts,
        ) -> Result<Arc<dyn sandbox::Sandbox>, sandbox::SandboxError> {
            Ok(self.0.clone())
        }
    }

    struct WriteModel(AtomicUsize);

    #[async_trait::async_trait]
    impl AgentModel for WriteModel {
        fn selected_model(&self, _: Role, _: Option<&str>) -> String {
            "effective-project-test".into()
        }

        async fn complete(
            &self,
            _: &runtime::AgentInvocationContext,
            _: Role,
            messages: &[providers::Message],
            _: &[providers::ToolSpec],
        ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
            let step = self.0.fetch_add(1, Ordering::SeqCst);
            let (content, finish_reason) = if step == 0 {
                (
                    providers::ContentBlock::ToolUse {
                        id: "write".into(),
                        name: "write".into(),
                        input: serde_json::json!({"path":"source.rs", "content":"// comment\nfn main() {}"}),
                    },
                    providers::FinishReason::ToolUse,
                )
            } else {
                assert_eq!(step, 1);
                assert!(
                    messages
                        .iter()
                        .flat_map(|message| &message.content)
                        .any(|block| {
                            matches!(
                                block,
                                providers::ContentBlock::ToolResult {
                                    is_error: false,
                                    ..
                                }
                            )
                        })
                );
                (
                    providers::ContentBlock::Text {
                        text: "done".into(),
                    },
                    providers::FinishReason::Stop,
                )
            };
            Ok(providers::ChatResponse {
                message: providers::Message {
                    role: providers::Role::Assistant,
                    content: vec![content],
                },
                finish_reason,
                usage: providers::Usage::default(),
            })
        }
    }

    #[tokio::test]
    async fn selected_project_controls_composed_write_and_cannot_disable_trusted_checker() {
        {
            let cwd = tempfile::tempdir().unwrap();
            let primary = tempfile::tempdir().unwrap();
            let repo_a = init_demo_repo(cwd.path()).unwrap();
            let repo_b = init_demo_repo(primary.path()).unwrap();
            let trusted = tempfile::tempdir().unwrap();
            let binary = trusted.path().join("checker");
            std::fs::write(
                &binary,
                "#!/bin/sh\ncat >/dev/null\nprintf 'warning' >&2\nexit 2\n",
            )
            .unwrap();
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::write(trusted.path().join("config.toml"), format!(
                "[comment_checker]\nenabled = true\nbinary = {:?}\ntimeout_ms = 2500\nprompt = 'trusted prompt'\n",
                binary.to_str().unwrap(),
            )).unwrap();
            std::fs::create_dir_all(repo_b.join(".evorch")).unwrap();
            std::fs::write(
                config::project_main_config_path(&repo_b),
                "[comment_checker]\nenabled = false\nbinary = '/untrusted/checker'\ntimeout_ms = 1\nprompt = 'untrusted prompt'\n",
            )
            .unwrap();
            let mut sidebar = SidebarState::default();
            for (id, root) in [("a", &repo_a), ("b", &repo_b)] {
                sidebar.add_project(ProjectId::new(id), id, root).unwrap();
            }
            sidebar.select_project(&ProjectId::new("b")).unwrap();
            let (root, mut load_options) = effective_project_context(&sidebar, &repo_a);
            assert_eq!(root, repo_b);
            load_options.user_config_dir = Some(trusted.path().to_path_buf());
            load_options.read_env = false;
            let config = config::Config::load(&load_options).unwrap();
            assert!(
                config.comment_checker.enabled,
                "projects cannot disable the trusted checker"
            );
            assert_eq!(config.comment_checker.binary, binary.to_str().unwrap());
            assert_eq!(config.comment_checker.timeout_ms, 2500);
            assert_eq!(
                config.comment_checker.prompt.as_deref(),
                Some("trusted prompt")
            );
            let bus = Arc::new(EventBus::new(64));
            let mut events = bus.subscribe();
            let sandbox = Arc::new(CountingSandbox(AtomicUsize::new(0)));
            let seam = WorkspaceSeam::with_factory(
                root.clone(),
                Arc::new(CountingFactory(sandbox.clone())),
            )
            .unwrap();
            let executor = Arc::new(tools::ToolExecutor::with_standard_tools_in(
                bus.clone(),
                sandbox.clone(),
                Some(root.clone()),
            ));
            let composed = compose_runtime(RuntimeComposition {
                config: &config,
                user_config_dir: Some(trusted.path().to_path_buf()),
                bus,
                executor,
                credential_store: Arc::new(UnwiredCredentialStore),
                env: Arc::new(routing::MapEnv::default()),
                model_source: ModelSource::Fixed(Arc::new(WriteModel(AtomicUsize::new(0)))),
                workspace: Some(seam),
            })
            .unwrap();
            let runtime = composed.runtime.with_sandbox_root(root);
            let run = runtime.delegate_background(
                Role::Worker,
                "write in primary".into(),
                RunConfig::default(),
            );
            assert_eq!(
                runtime.wait(run).await.unwrap(),
                event_bus::AgentRunPhase::Done
            );
            assert_eq!(sandbox.0.load(Ordering::SeqCst), 1);
            assert_eq!(
                std::fs::read_to_string(repo_b.join("source.rs")).unwrap(),
                "// comment\nfn main() {}"
            );
            assert!(!repo_a.join("source.rs").exists());
            loop {
                if let EventKind::Tool(event_bus::ToolEvent::ToolCompleted {
                    tool_name,
                    is_error,
                    detail,
                    ..
                }) = events.recv().await.unwrap().kind
                {
                    assert_eq!(tool_name, "write");
                    assert!(!is_error);
                    assert!(detail.is_some());
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod storage_shutdown_tests {
    use super::*;

    #[test]
    fn startup_resource_guard_keeps_the_host_alive_until_fenced_writes_drain() {
        let directory = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: directory.path().join("events.db"),
            ..Default::default()
        };
        let storage = Storage::open(config.clone()).unwrap();
        let db = Database::open(&config).unwrap();
        let bus = Arc::new(EventBus::new(64));
        let root = directory.path().join("owners");
        let host = Arc::new(
            runtime::ownership::OwnerHost::open(&root, Default::default(), bus.clone()).unwrap(),
        );
        let permit = host.start("thread").unwrap();
        let fence = permit.clone();
        let nonblocking_fence = permit.clone();
        bus.register_nonblocking_mutation_guard(
            "run".into(),
            Arc::new(move || {
                fence
                    .mutation_guard()
                    .ok()
                    .map(|guard| Box::new(guard) as Box<dyn event_bus::MutationGuard>)
            }),
            Arc::new(move || match nonblocking_fence.try_mutation_guard() {
                Ok(Some(guard)) => event_bus::MutationGuardAttempt::Acquired(Box::new(guard)),
                Ok(None) => event_bus::MutationGuardAttempt::Busy,
                Err(_) => event_bus::MutationGuardAttempt::Rejected,
            }),
        );
        // Match runtime registration: contention must defer a guarded batch,
        // not revoke a valid generation or wait while holding another guard.
        let probe = Event::new(event_bus::MessageEvent::MessageDelta {
            run_id: Some("run".into()),
            delta: "probe".into(),
        });
        let writer = rusqlite::Connection::open(root.join("owners.db")).unwrap();
        writer.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let validator = bus.mutation_validator();
        assert!(matches!(
            validator.acquire_batch(std::slice::from_ref(&probe)),
            Err(event_bus::MutationBatchError::Busy)
        ));
        writer.execute_batch("ROLLBACK").unwrap();
        assert_eq!(
            validator
                .acquire_batch(std::slice::from_ref(&probe))
                .unwrap()
                .accepted(),
            &[true]
        );
        let bridge = spawn_storage_bridge(
            bus.clone(),
            storage,
            "session",
            config::DiagnosticPersistence::Warnings,
            true,
            SharedUsagePricing::default(),
            None,
        )
        .unwrap();
        let monitor = bridge.monitor();
        let weak_host = Arc::downgrade(&host);
        let resources = GuiStorageResources {
            _storage: bridge,
            ownership: host.clone(),
            _demo_directory: None,
            _storage_fallback: None,
        };
        let event = Event::new(event_bus::MessageEvent::MessageDelta {
            run_id: Some("run".into()),
            delta: "tail before a later startup failure".into(),
        });
        bus.emit(event.clone());
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while monitor.snapshot().pending_events == 0 {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert_eq!(monitor.snapshot().persisted_events, 0);
        host.begin_quiesce().unwrap();
        // Simulate state/command-sink locals unwinding before a retained app
        // constructor is dropped without ever constructing a GUI window.
        drop(host);
        assert!(weak_host.upgrade().is_some());
        permit.validate_generation().unwrap();
        let constructor = move || resources;
        drop(constructor);
        assert!(weak_host.upgrade().is_none());
        let messages: Vec<_> = db
            .events_all_ordered()
            .unwrap()
            .into_iter()
            .filter(|row| matches!(row.event.kind, EventKind::Message(_)))
            .map(|row| row.event)
            .collect();
        assert_eq!(messages, [event]);
        let rows = db.events_all_ordered().unwrap();
        let delta_index = rows
            .iter()
            .position(|row| matches!(&row.event.kind, EventKind::Message(_)))
            .unwrap();
        let released_index = rows
            .iter()
            .position(|row| {
                matches!(&row.event.kind,
            EventKind::Ownership(owner) if owner.action == event_bus::OwnershipAction::Released)
            })
            .expect("Released must remain durable");
        assert!(delta_index < released_index);
        assert_eq!(monitor.snapshot().failed_events, 0);
        let registry =
            runtime::ownership::Registry::open_readonly(&root.join("owners.db")).unwrap();
        assert_eq!(
            registry.attach("thread").unwrap().state,
            runtime::ownership::OwnerState::Released
        );
        assert!(permit.validate_generation().is_err());
    }
}
