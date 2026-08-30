use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use event_bus::{Event, EventBus, LifecycleEvent};
use gui::app::{WorkbenchApp, WorkbenchState};
use gui::events::EventPump;
use gui::pty::PtySession;
use portable_pty::CommandBuilder;
use providers::{
    ChatResponse, ContentBlock, FinishReason, Message, Role as MessageRole, ToolSpec, Usage,
};
use runtime::{AgentModel, AgentRuntime, ExecutionPolicy, Role, RunConfig, RuntimeError};
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
    #[error("runtime initialization failed: {0}")]
    Runtime(#[from] runtime::RuntimeError),
    #[error("current directory lookup failed: {0}")]
    CurrentDir(#[from] std::io::Error),
}

/// 決定的な scripted model。
///
/// 外部 AI プロバイダなしで demo session を駆動する。会話履歴の先頭ユーザー
/// メッセージ本文を marker として script を選択する
/// (tests/runtime_wiring.rs の ScriptedModel と同じ dispatch 方式)。
struct DemoScriptModel {
    scripts: Mutex<HashMap<String, VecDeque<ChatResponse>>>,
}

impl Default for DemoScriptModel {
    fn default() -> Self {
        Self {
            scripts: Mutex::new(HashMap::from([
                (
                    "DEMO-ORCH".to_string(),
                    VecDeque::from([
                        tool_response(
                            "demo-delegate-w1",
                            "delegate_background",
                            serde_json::json!({
                                "role": "worker",
                                "prompt": "DEMO-W1",
                                "name": "worker-w1"
                            }),
                        ),
                        text_response("demo complete", FinishReason::Stop),
                    ]),
                ),
                (
                    "DEMO-W1".to_string(),
                    VecDeque::from([text_response("worker done", FinishReason::Stop)]),
                ),
            ])),
        }
    }
}

#[async_trait]
impl AgentModel for DemoScriptModel {
    async fn complete(
        &self,
        _role: Role,
        messages: &[Message],
        _tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        let marker = messages.first().and_then(|message| {
            message.content.iter().find_map(|block| match block {
                ContentBlock::Text { text } => Some(text.clone()),
                ContentBlock::Reasoning { .. }
                | ContentBlock::ToolUse { .. }
                | ContentBlock::ToolResult { .. } => None,
            })
        });
        let mut scripts = self.scripts.lock().expect("script lock must not poison");
        scripts
            .get_mut(marker.as_deref().unwrap_or_default())
            .and_then(VecDeque::pop_front)
            .ok_or_else(|| RuntimeError::Model {
                reason: format!("script exhausted for {marker:?}"),
            })
    }

    fn selected_model(&self, role: Role) -> String {
        format!("demo-{}", role.name().to_lowercase())
    }
}

fn text_response(text: &str, finish_reason: FinishReason) -> ChatResponse {
    response(
        vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        finish_reason,
    )
}

fn tool_response(id: &str, name: &str, input: serde_json::Value) -> ChatResponse {
    response(
        vec![ContentBlock::ToolUse {
            id: id.to_string(),
            name: name.to_string(),
            input,
        }],
        FinishReason::ToolUse,
    )
}

fn response(content: Vec<ContentBlock>, finish_reason: FinishReason) -> ChatResponse {
    ChatResponse {
        message: Message {
            role: MessageRole::Assistant,
            content,
        },
        usage: Usage::default(),
        finish_reason,
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
        "Usage: evorch-gui [--demo] [--settings PATH] [--layout PATH] [--save-layout PATH]

Demo mode (--demo) runs a deterministic scripted session; no external AI
provider is used or required.

Start:

    cargo run -p gui --bin evorch-gui -- --demo

Expected task rows (name / role / status / model):

    run-1: Orchestrator / Orchestrator / Done / demo-orchestrator
    run-2: worker-w1 / Worker / Done / demo-worker

Each row transitions Pending -> Running -> Done.

Requirements:

- `bwrap` must be on PATH. Without it the app prints
  `evorch-gui: runtime initialization failed: サンドボックス構築に失敗しました: ...`
  and exits with code 1."
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
    let bus = Arc::new(EventBus::new(EVENT_CAPACITY));
    let runtime = AgentRuntime::production(
        Arc::clone(&bus),
        &ExecutionPolicy::for_role(Role::Orchestrator),
        std::env::current_dir()?,
        Arc::new(DemoScriptModel::default()),
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
    let mut state = WorkbenchState::new(runtime.clone(), &settings)?
        .with_pump(pump)
        .with_pty(pty);
    if let Some(path) = arguments.save_layout {
        state = state.with_save_path(path);
    }

    if arguments.demo {
        bus.emit(Event::new(LifecycleEvent::Started {
            session_id: String::from("gui-demo"),
        }));
        let demo_runtime = runtime.clone();
        handle.spawn(async move {
            demo_runtime.delegate_background(
                Role::Orchestrator,
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
            Ok(Box::new(WorkbenchApp(state)))
        }),
    )
    .map_err(|error| GuiError::Eframe(error.to_string()))
}

fn main() {
    if let Err(error) = run() {
        eprintln!("evorch-gui: {error}");
        std::process::exit(1);
    }
}
