use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use event_bus::{Event, EventBus, LifecycleEvent};
use gui::app::{WorkbenchApp, WorkbenchState};
use gui::diff::FixtureDiffSource;
use gui::events::EventPump;
use gui::model::commands::FixtureLoopAdapter;
use gui::model::demo::DemoScriptModel;
use gui::pty::PtySession;
use gui::runtime_sink::RuntimeCommandSink;
use portable_pty::CommandBuilder;
use runtime::{AgentRuntime, ExecutionPolicy, Role, RunConfig};
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
    #[error("current directory lookup failed: {0}")]
    CurrentDir(#[from] std::io::Error),
    #[error("sidebar state failed: {0}")]
    Sidebar(#[from] workspace_ui::SidebarError),
    #[error("demo project state failed: {0}")]
    Project(#[from] workspace_ui::ProjectError),
    #[error("demo thread state failed: {0}")]
    Thread(#[from] workspace_ui::ThreadError),
    #[error("sidebar state directory initialization failed: {0}")]
    StateDirectory(std::io::Error),
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
    let runtime = AgentRuntime::production(
        Arc::clone(&bus),
        &ExecutionPolicy::for_role(Role::Orchestrator),
        repo_root.clone(),
        Arc::new(DemoScriptModel::new(Arc::clone(&bus))),
    )?;
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
    let pty = PtySession::spawn(CommandBuilder::new("/bin/sh"), 24, 80, None)?;
    // goal 投入を runtime の entry pre-routing 起動へ接続する production CommandSink。
    // demo モードでは下段で FixtureLoopAdapter へ差し替えられる。
    let mut state = WorkbenchState::new(runtime.clone(), &settings)?
        .with_pump(pump)
        .with_pty(pty)
        .with_command_sink(Box::new(RuntimeCommandSink::new(
            runtime.clone(),
            handle.clone(),
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
        state = state
            .with_diff_source(Arc::new(demo_diff_source()))
            .with_command_sink(Box::new(FixtureLoopAdapter::default()));
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
            }))
        }),
    )
    .map_err(|error| GuiError::Eframe(error.to_string()))
}

struct GuiApp {
    workbench: WorkbenchApp<AgentRuntime>,
    _demo_directory: Option<tempfile::TempDir>,
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
