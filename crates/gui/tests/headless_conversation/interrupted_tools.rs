//! Tool calls whose run ended without a result stop showing as running.

use event_bus::{AgentRunPhase, Event, LifecycleEvent, ToolEvent};
use gui::app::WorkbenchState;
use gui::headless::HeadlessWorkbench;
use gui::model::tasks::AgentRunSource;
use gui::model::transcript::{ToolStatus, TranscriptEntry};
use runtime::{AgentInspection, AgentSummary, RunId};
use workspace_ui::{ProjectId, SidebarState, ThreadId, UiSettings};

struct EmptySource;

impl AgentRunSource for EmptySource {
    fn list(&self) -> Vec<AgentSummary> {
        Vec::new()
    }
    fn inspect(&self, _: RunId) -> Option<AgentInspection> {
        None
    }
}

const RUN: &str = "run-target";

fn state(root: &std::path::Path) -> WorkbenchState<EmptySource> {
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("p");
    sidebar
        .add_project(project.clone(), "Project", root)
        .unwrap();
    sidebar.select_project(&project).unwrap();
    sidebar
        .create_thread(ThreadId::new("target"), project, "Target")
        .unwrap();
    sidebar.switch_thread(&ThreadId::new("target")).unwrap();
    let mut json = serde_json::to_value(sidebar).unwrap();
    json["threads"][0]["run_ids"] = serde_json::json!([RUN]);
    WorkbenchState::new(EmptySource, &UiSettings::default())
        .unwrap()
        .with_sidebar(serde_json::from_value(json).unwrap())
        .with_sidebar_path(root.join("sidebar.json"))
}

/// A run whose process exited mid-call: the call started, no completion or
/// terminal phase was recorded, and the snapshot says where it stopped.
fn persist_unfinished_call(storage: &storage::Storage) {
    for event in [
        Event::new(LifecycleEvent::AgentRunStarted {
            run_id: RUN.into(),
            parent_run_id: None,
            agent_name: "chat:Worker:target".into(),
            role: "Worker".into(),
        }),
        Event::new(LifecycleEvent::AgentRunStateChanged {
            run_id: RUN.into(),
            from: AgentRunPhase::Pending,
            to: AgentRunPhase::Running,
            reason: None,
        }),
        Event::new(ToolEvent::ToolStarted {
            tool_name: "shell".into(),
            call_id: "call-lost".into(),
            input: Some(serde_json::json!({"command": "sleep 600"})),
            run_id: Some(RUN.into()),
        }),
    ] {
        storage.handle().append_event(Some("gui"), &event).unwrap();
    }
    storage
        .handle()
        .upsert_run_context(&storage::RunContextRecord {
            run_id: RUN.into(),
            role: "Worker".into(),
            name: "chat:Worker:target".into(),
            parent_run_id: None,
            config_json: "{}".into(),
            messages_json: "[]".into(),
            checkpoints_json: "[]".into(),
            terminal_phase: "Checkpoint".into(),
            restorable: true,
            updated_at_ns: 1,
        })
        .unwrap();
}

fn tool_status(state: &WorkbenchState<EmptySource>) -> Option<ToolStatus> {
    state
        .transcript()
        .entries()
        .iter()
        .find_map(|entry| match entry {
            TranscriptEntry::Tool {
                call_id, status, ..
            } if call_id == "call-lost" => Some(status.clone()),
            _ => None,
        })
}

#[test]
fn a_restored_orphan_run_interrupts_its_unfinished_call() {
    // Given: a recorded run that exited mid-call, with no live owner.
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("events.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    persist_unfinished_call(&storage);
    storage.close();
    let registry = dir.path().join("owners.db");
    drop(runtime::ownership::Registry::open(&registry).unwrap());
    let database = storage::Database::open(&config).unwrap();

    // When: history is restored without an ownership registry, the call keeps
    // running because nothing proves that the run ended.
    let mut unknown = state(dir.path());
    unknown.restore_history(&database).unwrap();
    assert_eq!(tool_status(&unknown), Some(ToolStatus::Running));

    // When: ownership shows the run is orphaned and reconciles it as stopped.
    let mut restored = state(dir.path());
    restored
        .restore_history_with_ownership(&database, &registry)
        .unwrap();

    // Then: its unfinished call is interrupted, and the card says so without a spinner.
    assert_eq!(
        restored.thread_phases()[RUN],
        workspace_ui::ThreadRunPhase::Stopped
    );
    assert_eq!(tool_status(&restored), Some(ToolStatus::Interrupted));
    let mut harness = HeadlessWorkbench::new(restored, [1000.0, 700.0]);
    for _ in 0..3 {
        harness.step();
    }
    let label = format!(
        "⏹ Shell sleep 600 · {}",
        gui::panes::transcript_tool::INTERRUPTED_META
    );
    assert!(harness.has_label(&label), "missing {label:?}");
}
