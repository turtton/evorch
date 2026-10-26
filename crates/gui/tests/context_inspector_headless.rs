//! The Context tab previews, compares and reads run contexts through the command sink.

use std::sync::{Arc, Mutex};

use gui::fixture::DemoSource;
use gui::model::commands::{CommandSink, ContextPreviewReceiver, LoopEvent, WorkbenchCommand};
use gui::panes::context_inspector::InspectorMode;
use providers::{ContentBlock, Message, Role as MessageRole};
use runtime::Role;
use runtime::base_context::{
    BaseContextReport, BaseContextRequest, ContextExclusion, ContextSection, ContextSectionKind,
    ContextSource, ContextTool, RunContextView,
};

type Requests = Arc<Mutex<Vec<BaseContextRequest>>>;

struct StubSink(Requests);

fn report(role: Role) -> BaseContextReport {
    let mut sections = vec![ContextSection {
        kind: ContextSectionKind::RoleBaseline,
        source: ContextSource::BundledPreset {
            name: format!("role-{}", role.name().to_lowercase()),
        },
        text: format!("{} baseline body", role.name()),
        estimated_tokens: 3,
    }];
    if role == Role::Orchestrator {
        sections.push(ContextSection {
            kind: ContextSectionKind::IntentGate,
            source: ContextSource::Generated,
            text: "## Intent Gate".into(),
            estimated_tokens: 4,
        });
    }
    let tool = if role == Role::Orchestrator {
        "delegate"
    } else {
        "shell"
    };
    BaseContextReport {
        role,
        category: None,
        selected_model: "main/model-x".into(),
        context_window: 1_000,
        window_source: event_bus::WindowSource::Default,
        binding: None,
        sections,
        tools: vec![ContextTool {
            name: tool.into(),
            description: format!("{tool} description"),
            input_schema: serde_json::json!({"type": "object"}),
            estimated_tokens: 5,
        }],
        excluded_tools: Vec::new(),
        exclusions: vec![ContextExclusion {
            item: "Task prompt".into(),
            reason: "Sent separately.".into(),
        }],
        available_skills: Vec::new(),
        reasoning_effort: None,
    }
}

impl CommandSink for StubSink {
    fn preview_base_context(&self, request: BaseContextRequest) -> Option<ContextPreviewReceiver> {
        let (sender, receiver) = std::sync::mpsc::channel();
        sender.send(Ok(report(request.role))).unwrap();
        self.0.lock().unwrap().push(request);
        Some(receiver)
    }

    fn run_context_view(&self, run: &str) -> Result<Option<RunContextView>, String> {
        let messages = saved_messages(run);
        Ok(Some(RunContextView {
            run_id: runtime::RunId::new(run.trim_start_matches("run-").parse().unwrap()),
            role_name: "Worker".into(),
            category: Some("deep".into()),
            checkpoint_phase: "Done".into(),
            updated_at_ns: 0,
            selected_model: Some("main/model-x".into()),
            tool_names: vec!["shell".into()],
            messages,
            compaction_checkpoint_count: 0,
        }))
    }

    fn submit(&mut self, _: WorkbenchCommand) -> Vec<LoopEvent> {
        Vec::new()
    }
}

fn harness(requests: Requests) -> gui::headless::HeadlessWorkbench<DemoSource> {
    let state =
        gui::app::WorkbenchState::new(DemoSource(Vec::new()), &workspace_ui::UiSettings::default())
            .expect("workbench")
            .with_command_sink(Box::new(StubSink(requests)));
    gui::headless::HeadlessWorkbench::new(state, [1400.0, 1000.0])
}

/// The stub answers synchronously, so a bounded number of frames settles every request.
fn settle(harness: &mut gui::headless::HeadlessWorkbench<DemoSource>) {
    for _ in 0..4 {
        harness.step();
    }
}

#[test]
fn settings_menu_opens_a_preview_with_sections_tools_and_exclusions() {
    let requests = Requests::default();
    let mut harness = harness(requests.clone());
    harness.step();

    harness.click_label("⚙");
    harness.run();
    harness.click_label("Agent context");
    harness.run();
    settle(&mut harness);

    assert_eq!(requests.lock().unwrap()[0].role, Role::Orchestrator);
    assert!(harness.has_label("Role baseline  ·  3"));
    assert!(harness.has_label("Intent gate  ·  4"));
    assert!(harness.has_label("delegate  ·  5"));
    assert!(harness.has_label("Not included (1)"));

    harness.click_label("Role baseline  ·  3");
    settle(&mut harness);
    assert!(harness.has_label("bundled preset role-orchestrator · 3 tokens (estimate)"));
}

#[test]
fn compare_lines_up_orchestrator_and_worker() {
    let requests = Requests::default();
    let mut harness = harness(requests.clone());
    harness
        .state_mut()
        .open_context_tab(InspectorMode::Compare, None);
    settle(&mut harness);

    let roles: Vec<_> = requests
        .lock()
        .unwrap()
        .iter()
        .map(|request| (request.role, request.child))
        .collect();
    assert_eq!(roles, [(Role::Orchestrator, false), (Role::Worker, true)]);
    assert!(harness.has_label("Tools only Orchestrator sees"));
    assert!(harness.has_label("delegate"));
    assert!(harness.has_label("differs"));
}

#[test]
fn run_view_lists_the_saved_messages() {
    let mut harness = harness(Requests::default());
    harness
        .state_mut()
        .open_context_tab(InspectorMode::Run, Some("run-7".into()));
    settle(&mut harness);

    assert!(harness.has_label("Messages (2)"));
    assert!(harness.has_label(
        "Worker · deep · main/model-x · 2 messages · 0 compaction checkpoints · saved at Done"
    ));
}

const NATIVE_RUN: &str = "run-9";

/// The history the stub saved for `run`; [`NATIVE_RUN`] ends with a native-only reply.
fn saved_messages(run: &str) -> Vec<Message> {
    let mut messages = vec![
        Message {
            role: MessageRole::System,
            content: vec![ContentBlock::Text {
                text: "SYSTEM TEXT".into(),
            }],
        },
        Message {
            role: MessageRole::User,
            content: vec![ContentBlock::Text {
                text: "TASK TEXT".into(),
            }],
        },
    ];
    if run == NATIVE_RUN {
        messages.push(native_only_reply());
    }
    messages
}

/// An assistant reply that holds only Responses native state.
fn native_only_reply() -> Message {
    Message {
        role: MessageRole::Assistant,
        content: vec![ContentBlock::ResponsesReasoning {
            item: serde_json::json!({
                "type": "reasoning", "id": "rs_headless", "summary": [],
                "encrypted_content": "gAAAAB3-headless-ciphertext"
            }),
            origin: providers::ResolvedResponsesScope {
                dialect: providers::ResponsesDialect::Codex,
                profile: String::new(),
                route_model: "gpt-5.1-codex".into(),
                wire_model: "gpt-5.1-codex".into(),
                speed: providers::ResponsesSpeed::Standard,
                service_tier: None,
                endpoint_fingerprint: "ab".repeat(32),
                account: providers::ResponsesAccount::Unavailable,
            },
        }],
    }
}

#[test]
fn run_view_budgets_native_only_replies_without_listing_an_empty_row() {
    // Given: a saved history whose last reply is native Responses state only.
    let mut harness = harness(Requests::default());
    harness
        .state_mut()
        .open_context_tab(InspectorMode::Run, Some(NATIVE_RUN.into()));

    // When: the run view is rendered.
    settle(&mut harness);

    // Then: header and list both count saved messages; the list states how many
    // of them it shows, explains the hidden one, and shows no empty row for it.
    use gui::model::context_inspector::{compact_tokens, message_rows};
    let rows = message_rows(&saved_messages(NATIVE_RUN));
    let empty_row = format!("assistant · {} · ", compact_tokens(rows[2].tokens));
    assert!(harness.has_label(
        "Worker · deep · main/model-x · 3 messages · 0 compaction checkpoints · saved at Done"
    ));
    assert!(harness.has_label("Messages (2 of 3)"));
    assert!(!harness.has_label("Messages (2)"));
    assert!(harness.has_label("1 replay-only message hidden · counted in the budget"));
    assert!(!harness.has_label(&empty_row));
    assert!(!harness.has_label(empty_row.trim_end()));

    // And: the saved-history budget still includes the hidden reply's tokens.
    let total: u64 = rows.iter().map(|row| row.tokens).sum();
    assert!(rows[2].tokens > 0);
    assert!(harness.has_label(&format!("Saved history {}", compact_tokens(total))));
}

/// Real runtime composition behind a sink, for screenshots of bundled presets and tools.
struct LiveSink {
    runtime: runtime::AgentRuntime,
    tokio: tokio::runtime::Runtime,
}

struct FixedModel;

#[async_trait::async_trait]
impl runtime::AgentModel for FixedModel {
    async fn complete(
        &self,
        _invocation: &runtime::AgentInvocationContext,
        _role: Role,
        _messages: &[Message],
        _tools: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
        Err(runtime::RuntimeError::Model {
            reason: "preview only".into(),
        })
    }

    fn selected_model(&self, _role: Role, _category: Option<&str>) -> String {
        "anthropic/claude-sonnet-5".into()
    }
}

impl LiveSink {
    fn new(user_dir: &std::path::Path, project: &std::path::Path) -> Self {
        let bus = Arc::new(event_bus::EventBus::new(64));
        let executor = Arc::new(
            tools::ToolExecutor::with_standard_tools(
                Arc::clone(&bus),
                Arc::new(sandbox::DirectSandbox::new_unchecked()),
            )
            .with_web_tools()
            .unwrap(),
        );
        let source = runtime::SkillCatalogSource::new(
            config::Config::default(),
            Some(user_dir.to_path_buf()),
            Vec::new(),
            Vec::new(),
            Arc::clone(&bus),
        );
        let runtime = runtime::AgentRuntime::new(bus, executor, Arc::new(FixedModel))
            .with_skill_source(Arc::new(source))
            .with_project_rules(Arc::new(runtime::RulesSource::new(
                runtime::ProjectTrust::Unapproved,
                runtime::RulesSettings::from(&config::RulesConfig::default()),
                None,
                Some(project.to_path_buf()),
                Some(user_dir.join("AGENTS.md")),
            )));
        Self {
            runtime,
            tokio: tokio::runtime::Runtime::new().unwrap(),
        }
    }
}

impl CommandSink for LiveSink {
    fn preview_base_context(&self, request: BaseContextRequest) -> Option<ContextPreviewReceiver> {
        let (sender, receiver) = std::sync::mpsc::channel();
        let report = self
            .tokio
            .block_on(self.runtime.preview_base_context(request))
            .map_err(|error| error.to_string());
        sender.send(report).unwrap();
        Some(receiver)
    }

    fn submit(&mut self, _: WorkbenchCommand) -> Vec<LoopEvent> {
        Vec::new()
    }
}

#[test]
#[ignore = "writes PNG evidence using an offscreen GPU adapter"]
fn capture_context_tab_png_evidence() {
    let user_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        user_dir.path().join("AGENTS.md"),
        "# Personal rules\n\n- Reply in Japanese.\n- Prefer small commits.\n",
    )
    .unwrap();
    std::fs::write(project.path().join("AGENTS.md"), "# Project rules\n").unwrap();
    let state =
        gui::app::WorkbenchState::new(DemoSource(Vec::new()), &workspace_ui::UiSettings::default())
            .expect("workbench")
            .with_command_sink(Box::new(LiveSink::new(user_dir.path(), project.path())));
    let mut harness = gui::headless::HeadlessWorkbench::new(state, [1400.0, 1000.0]);
    let output =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/gui-evidence/context");
    std::fs::create_dir_all(&output).expect("evidence directory");
    let save = |harness: &mut gui::headless::HeadlessWorkbench<DemoSource>, name: &str| {
        harness.pointer_move(egui::pos2(1390.0, 990.0));
        harness.run();
        if let Some(frame) = gui::evidence::capture_or_skip(harness) {
            frame
                .save_png(&output.join(format!("context-{name}.png")))
                .expect("context PNG");
        }
    };
    harness
        .state_mut()
        .open_context_tab(InspectorMode::Preview, None);
    settle(&mut harness);
    save(&mut harness, "preview");
    harness
        .state_mut()
        .open_context_tab(InspectorMode::Compare, None);
    settle(&mut harness);
    save(&mut harness, "compare");
}

#[test]
fn saved_oracle_context_is_displayed_as_advisor() {
    let sink = StubSink(Arc::new(Mutex::new(Vec::new())));
    for legacy in ["Oracle", "oracle"] {
        let mut view = sink.run_context_view("run-1").unwrap().unwrap();
        view.role_name = legacy.into();
        assert!(gui::model::context_inspector::run_summary(&view).starts_with("Advisor"));
    }
    assert!(gui::model::context_inspector::ROLES.contains(&Role::Advisor));
    assert!(gui::model::context_inspector::categories_for(Role::Advisor).is_empty());
}

#[test]
fn switching_active_thread_requests_a_new_thread_specific_preview() {
    let requests = Requests::default();
    let mut harness = harness(requests.clone());
    let repo = tempfile::tempdir().unwrap();
    let state = harness.state_mut();
    let project = state.add_project(repo.path()).unwrap();
    state.select_project(project).unwrap();
    let first = state.create_thread("First").unwrap();
    let second = state.create_thread("Second").unwrap();
    state.switch_thread(first.clone()).unwrap();
    state.open_context_tab(InspectorMode::Preview, None);
    settle(&mut harness);
    assert_eq!(
        requests.lock().unwrap().last().unwrap().thread_id,
        Some(first.to_string())
    );
    requests.lock().unwrap().clear();
    harness.state_mut().switch_thread(second.clone()).unwrap();
    settle(&mut harness);
    let sent = requests.lock().unwrap();
    assert_eq!(
        sent.len(),
        1,
        "same role/project must not reuse another thread's preview"
    );
    assert_eq!(sent[0].thread_id, Some(second.to_string()));
}
