use std::path::PathBuf;
use std::sync::Arc;

use event_bus::{Event, EventBus, LifecycleEvent};
use gui::app::{WorkbenchApp, WorkbenchState};
use gui::events::EventPump;
use gui::pty::PtySession;
use portable_pty::CommandBuilder;
use runtime::AgentSummary;
use workspace_ui::UiSettings;

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
}

#[derive(Debug, Default)]
struct EmptyAgentSource;

impl gui::model::tasks::AgentRunSource for EmptyAgentSource {
    fn list(&self) -> Vec<AgentSummary> {
        Vec::new()
    }
}

#[derive(Debug, Default)]
struct Arguments {
    demo: bool,
    settings: Option<PathBuf>,
    layout: Option<PathBuf>,
    save_layout: Option<PathBuf>,
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
            "--help" | "-h" => {
                println!(
                    "Usage: evorch-gui [--demo] [--settings PATH] [--layout PATH] [--save-layout PATH]"
                );
                std::process::exit(0);
            }
            unknown => return Err(GuiError::Arguments(format!("unknown option: {unknown}"))),
        }
    }
    Ok(arguments)
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

fn spawn_event_bridge(bus: Arc<EventBus>) -> Result<EventPump, GuiError> {
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
            let pump = EventPump::spawn(&runtime.handle().clone(), bus.subscribe(), None);
            if pump_sender.send(pump).is_err() {
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
    let bus = Arc::new(EventBus::new(EVENT_CAPACITY));
    let pump = spawn_event_bridge(Arc::clone(&bus))?;
    let pty = PtySession::spawn(CommandBuilder::new("/bin/sh"), 24, 80, None)?;
    let mut state = WorkbenchState::new(EmptyAgentSource, &settings, "runtime")?
        .with_pump(pump)
        .with_pty(pty);
    if let Some(path) = arguments.save_layout {
        state = state.with_save_path(path);
    }

    if arguments.demo {
        bus.emit(Event::new(LifecycleEvent::Started {
            session_id: String::from("gui-demo"),
        }));
    }

    let options = eframe::NativeOptions::default();
    eframe::run_native(
        "evorch",
        options,
        Box::new(move |_creation_context| Ok(Box::new(WorkbenchApp(state)))),
    )
    .map_err(|error| GuiError::Eframe(error.to_string()))
}

fn main() {
    if let Err(error) = run() {
        eprintln!("evorch-gui: {error}");
        std::process::exit(1);
    }
}
