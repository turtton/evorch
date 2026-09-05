use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use event_bus::{Event, EventBus, EventKind, LifecycleEvent, RecvError};
use gui::app::{WorkbenchApp, WorkbenchState};
use gui::diff::FixtureDiffSource;
use gui::events::EventPump;
use gui::model::demo::DemoScriptModel;
use gui::pty::PtySession;
use gui::runtime_sink::{
    RuntimeCommandSink, STORAGE_SESSION_ID, derive_base_ref, derive_repo_slug,
};
use portable_pty::CommandBuilder;
use runtime::orchestration::delivery::DeliveryPort;
use runtime::{
    AgentRuntime, ExecutionPolicy, FixtureDeliveryAdapter, GoalLedger, GoalSupervisor,
    OrchestrationSettings, Role, RunConfig, ShellDeliveryAdapter, SupervisorHandle,
};
use sandbox::{BwrapConfig, Sandbox, production_sandbox};
use storage::{Database, Storage, StorageConfig, StorageHandle};
use workspace_ui::{ProjectId, SidebarState, ThreadId, TrustState, UiSettings};

const EVENT_CAPACITY: usize = 256;

#[derive(Debug, thiserror::Error)]
enum GuiError {
    #[error("argument error: {0}")]
    Arguments(String),
    #[error("settings load failed: {0}")]
    Settings(#[from] workspace_ui::SettingsError),
    #[error("layout load failed: {0}")]
    Layout(#[from] workspace_ui::PersistError),
    #[error("workbench initialization failed: {0}")]
    Workbench(#[from] gui::app::WorkbenchError),
    #[error("PTY initialization failed: {0}")]
    Terminal(#[from] gui::pty::TerminalError),
    #[error("GUI initialization failed: {0}")]
    Eframe(String),
    #[error("runtime initialization failed: {0}")]
    Runtime(#[from] runtime::RuntimeError),
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
}

#[derive(Debug, Default)]
struct Arguments {
    demo: bool,
    settings: Option<PathBuf>,
    layout: Option<PathBuf>,
    save_layout: Option<PathBuf>,
    state: Option<PathBuf>,
}

fn parse_arguments() -> Result<Arguments, GuiError> {
    let mut arguments = Arguments::default();
    let mut values = std::env::args().skip(1);
    while let Some(argument) = values.next() {
        match argument.as_str() {
            "--demo" => arguments.demo = true,
            "--settings" => arguments.settings = Some(next_path(&mut values, "--settings")?),
            "--layout" => arguments.layout = Some(next_path(&mut values, "--layout")?),
            "--save-layout" => {
                arguments.save_layout = Some(next_path(&mut values, "--save-layout")?);
            }
            "--state" => arguments.state = Some(next_path(&mut values, "--state")?),
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
        r#"Usage: evorch-gui [--demo] [--settings PATH] [--layout PATH] [--save-layout PATH] [--state PATH]

Demo mode (--demo) runs a deterministic scripted session; no external AI
provider is used or required.

Non-demo mode starts a real AgentRuntime. Goal submission goes through the
entry router and launches a pre-routed background run: an explicit "direct"
keyword starts a Worker run directly; otherwise an Orchestrator run is
started.
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
6. Goal: submit and confirm "accepted: goal-1".
7. Merge: PR #65 is pending; Approve resolves it and disables the buttons.
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
    let mut settings = arguments
        .settings
        .as_deref()
        .map(workspace_ui::load_settings)
        .transpose()?
        .unwrap_or_default();
    if let Some(layout) = arguments.layout.as_deref() {
        settings.layout.workspace = Some(workspace_ui::load_workspace(layout)?);
    }
    Ok(settings)
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
                "--allow-empty",
                "--quiet",
                "-m",
                "initial demo commit",
            ],
        ),
    ];
    for (label, args) in commands {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(&repo)
            .output()?;
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

/// bus 上の全イベントを storage へ永続化する bridge を専用スレッドで起動する。
///
/// `append_event` は writer スレッドの応答を同期待ちするため、event pump と
/// 同じ runtime で動かすと他 task を block する。専用スレッド + current-thread
/// runtime に隔離する。
fn spawn_storage_bridge(
    bus: Arc<EventBus>,
    storage: StorageHandle,
    session_id: &'static str,
) -> Result<(), GuiError> {
    std::thread::Builder::new()
        .name(String::from("evorch-storage-bridge"))
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    tracing::error!(%error, "storage bridge runtime failed");
                    return;
                }
            };
            runtime.block_on(async move {
                let mut subscriber = bus.subscribe();
                loop {
                    match subscriber.recv().await {
                        Ok(event) => {
                            if let Err(error) = storage.append_event(Some(session_id), &event) {
                                tracing::warn!(%error, "failed to persist event");
                            }
                        }
                        Err(RecvError::Lagged(skipped)) => {
                            tracing::warn!(skipped, "storage bridge lagged");
                        }
                        Err(RecvError::Closed) => return,
                    }
                }
            });
        })
        .map_err(|error| GuiError::Arguments(format!("storage bridge thread failed: {error}")))?;
    Ok(())
}

/// 前セッションの goal 状態を永続化イベントから復元し、supervisor へ移管する。
///
/// `Database::events_all_ordered()` → `GoalLedger::replay` で goal ごとの
/// snapshot を再構築し、transcript を `agent_messages_by_session` で付与して
/// `adopt` する。Active goal は supervisor 側で Paused
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
    let goals = GoalLedger::replay(orchestrator_events)
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
    loaded: Result<config::Config, config::ConfigError>,
) -> OrchestrationSettings {
    match loaded {
        Ok(config) => OrchestrationSettings::from(&config.orchestration),
        Err(error) => {
            tracing::warn!(%error, "config load failed; using default orchestration settings");
            OrchestrationSettings::default()
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
            let runtime = match tokio::runtime::Builder::new_current_thread().build() {
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

fn run() -> Result<(), GuiError> {
    let arguments = parse_arguments()?;
    let settings = load_settings(&arguments)?;
    let repo_root = std::fs::canonicalize(std::env::current_dir()?)?;
    let state_path = sidebar_path(&arguments);
    if let Some(parent) = state_path.as_deref().and_then(std::path::Path::parent) {
        std::fs::create_dir_all(parent).map_err(GuiError::StateDirectory)?;
    }
    let demo_directory = arguments.demo.then(tempfile::tempdir).transpose()?;
    let bus = Arc::new(EventBus::new(EVENT_CAPACITY));

    let runtime = match demo_directory.as_ref() {
        Some(directory) => {
            let demo_repo = init_demo_repo(directory.path())?;
            AgentRuntime::production_with_project(
                Arc::clone(&bus),
                &ExecutionPolicy::for_role(Role::Orchestrator),
                demo_repo,
                Arc::new(DemoScriptModel::new(Arc::clone(&bus))),
            )?
        }
        None => AgentRuntime::production(
            Arc::clone(&bus),
            &ExecutionPolicy::for_role(Role::Orchestrator),
            repo_root.clone(),
            Arc::new(DemoScriptModel::new(Arc::clone(&bus))),
        )?,
    };

    let (storage_db_path, storage_fallback) = storage_db_path(demo_directory.as_ref())?;
    let storage_config = StorageConfig {
        db_path: storage_db_path,
        ..StorageConfig::default()
    };
    let storage = Storage::open(storage_config.clone())?;

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

    // --demo は常に既定値を使い、非 demo のみ config 読み込みを試みる
    // (計画 Clarification C)。
    let orchestration = if arguments.demo {
        OrchestrationSettings::default()
    } else {
        orchestration_settings_or_default(config::Config::load(&config::LoadOptions::default()))
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

    spawn_storage_bridge(Arc::clone(&bus), storage.handle(), STORAGE_SESSION_ID)?;
    restore_goals(&storage_config, &supervisor);

    let pty = PtySession::spawn(CommandBuilder::new("/bin/sh"), 24, 80, None)?;
    // goal 投入から run 起動・supervisor 登録・merge/pause/resume/cancel までを
    // production 経路で接続する CommandSink (demo も同様)。
    let mut state = WorkbenchState::new(runtime.clone(), &settings)?
        .with_pump(pump)
        .with_pty(pty)
        .with_command_sink(Box::new(RuntimeCommandSink::new(
            runtime.clone(),
            handle.clone(),
            supervisor,
        )));
    let sidebar = match demo_directory.as_ref() {
        Some(directory) => demo_sidebar(&repo_root, directory.path())?,
        None => load_sidebar(state_path.as_ref())?,
    };
    state = state.with_sidebar(sidebar);
    if let Some(path) = state_path {
        state = state.with_sidebar_path(path);
    }
    if let Some(path) = arguments.save_layout {
        state = state.with_save_path(path);
    }

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

    let options = eframe::NativeOptions::default();
    eframe::run_native(
        "evorch",
        options,
        Box::new(move |creation_context| {
            let _ = repaint_ctx.set(creation_context.egui_ctx.clone());
            Ok(Box::new(GuiApp {
                workbench: WorkbenchApp(state),
                _demo_directory: demo_directory,
                _storage: storage,
                _storage_fallback: storage_fallback,
            }))
        }),
    )
    .map_err(|error| GuiError::Eframe(error.to_string()))
}

struct GuiApp {
    workbench: WorkbenchApp<AgentRuntime>,
    _demo_directory: Option<tempfile::TempDir>,
    _storage: Storage,
    _storage_fallback: Option<tempfile::TempDir>,
}

impl eframe::App for GuiApp {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
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

    // Given: config 読み込みが失敗したとき
    // When: orchestration 設定を解決する
    // Then: 既定値へ fallback する
    #[test]
    fn orchestration_settings_fall_back_to_default_on_config_error() {
        assert_eq!(
            orchestration_settings_or_default(Err(ConfigError::Migration("test".to_owned()))),
            OrchestrationSettings::default()
        );
    }
}
