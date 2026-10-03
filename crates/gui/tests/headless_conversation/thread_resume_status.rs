use event_bus::{AgentRunPhase, Event, LifecycleEvent};
use gui::app::WorkbenchState;
use gui::fixture::DemoSource;
use gui::headless::HeadlessWorkbench;
use gui::model::commands::{CommandSink, LoopEvent, WorkbenchCommand};
use gui::model::composer::ProviderStatus;
use storage::{Database, Storage, StorageConfig};
use workspace_ui::{ProjectId, SidebarState, ThreadId, UiSettings};

struct ContinueSink;

impl CommandSink for ContinueSink {
    fn submit(&mut self, command: WorkbenchCommand) -> Vec<LoopEvent> {
        let WorkbenchCommand::ContinueChat(request) = command else {
            panic!("expected /continue");
        };
        vec![LoopEvent::ChatAccepted {
            thread_id: request.thread_id,
            run_id: "root".into(),
        }]
    }
}

fn state(root: &std::path::Path) -> WorkbenchState<DemoSource> {
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("project");
    sidebar
        .add_project(project.clone(), "Project", root)
        .unwrap();
    sidebar.select_project(&project).unwrap();
    sidebar
        .create_thread(ThreadId::new("thread"), project, "Thread")
        .unwrap();
    sidebar.switch_thread(&ThreadId::new("thread")).unwrap();
    WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar)
        .with_provider_status(ProviderStatus::Configured)
        .with_command_sink(Box::new(ContinueSink))
}

fn started(run: &str, parent: Option<&str>) -> Event {
    Event::new(LifecycleEvent::AgentRunStarted {
        run_id: run.into(),
        parent_run_id: parent.map(str::to_owned),
        agent_name: if parent.is_none() {
            "chat:Worker:thread".into()
        } else {
            "worker".into()
        },
        role: "worker".into(),
    })
}

fn phase(run: &str, from: AgentRunPhase, to: AgentRunPhase) -> Event {
    Event::new(LifecycleEvent::AgentRunStateChanged {
        run_id: run.into(),
        from,
        to,
        reason: None,
    })
}

fn assert_status(harness: &mut HeadlessWorkbench<DemoSource>, phase: AgentRunPhase) {
    // Running spinners continuously repaint; only advance a bounded number of frames.
    harness.step();
    harness.step();
    assert_eq!(
        harness.has_label("Thread status: Error"),
        phase == AgentRunPhase::Error
    );
    assert_eq!(
        harness.has_label("Thread status: Running"),
        matches!(phase, AgentRunPhase::Pending | AgentRunPhase::Running)
    );
}

#[test]
fn continue_clears_error_status_without_erasing_failed_child_history() {
    for with_child in [false, true] {
        // Given: a failed conversation, optionally with a failed child.
        let dir = tempfile::tempdir().unwrap();
        let mut harness = HeadlessWorkbench::new(state(dir.path()), [1200.0, 900.0]);
        harness.state_mut().apply_events([started("root", None)]);
        if with_child {
            harness.state_mut().apply_events([
                started("child", Some("root")),
                phase("child", AgentRunPhase::Running, AgentRunPhase::Error),
            ]);
        }
        harness.state_mut().apply_events([phase(
            "root",
            AgentRunPhase::Running,
            AgentRunPhase::Error,
        )]);
        assert_status(&mut harness, AgentRunPhase::Error);

        // When: /continue is accepted and the same root is registered again.
        harness.state_mut().composer_mut().input = "/continue".into();
        harness.state_mut().submit_composer();
        assert!(matches!(
            harness.state().issued(),
            [WorkbenchCommand::ContinueChat(_)]
        ));
        // Acceptance alone cannot hide an error before runtime confirms resumption.
        assert_status(&mut harness, AgentRunPhase::Error);
        harness.state_mut().apply_events([
            started("root", None),
            phase("root", AgentRunPhase::Pending, AgentRunPhase::Pending),
        ]);
        assert_status(&mut harness, AgentRunPhase::Pending);

        // Then: the thread follows the resumed root, including a subsequent stop/error.
        let mut previous = AgentRunPhase::Pending;
        for next in [
            AgentRunPhase::Running,
            AgentRunPhase::Waiting,
            AgentRunPhase::Stopped,
            AgentRunPhase::Pending,
            AgentRunPhase::Running,
            AgentRunPhase::Done,
            AgentRunPhase::Error,
        ] {
            harness
                .state_mut()
                .apply_events([phase("root", previous, next)]);
            assert_status(&mut harness, next);
            previous = next;
        }
        let expected = if with_child {
            vec!["root", "child"]
        } else {
            vec!["root"]
        };
        assert_eq!(harness.state().sidebar().threads[0].run_ids, expected);
        assert_eq!(
            harness.state().transcripts().run("child").is_some(),
            with_child
        );
    }
}

#[test]
fn new_conversation_root_supersedes_old_error_and_stopped_runs() {
    for terminal in [AgentRunPhase::Error, AgentRunPhase::Stopped] {
        let dir = tempfile::tempdir().unwrap();
        let mut harness = HeadlessWorkbench::new(state(dir.path()), [1200.0, 900.0]);
        harness.state_mut().apply_events([
            started("old-root", None),
            phase("old-root", AgentRunPhase::Running, terminal),
            started("root", None),
            phase("root", AgentRunPhase::Pending, AgentRunPhase::Running),
            // Late events from the old incarnation must not change the current status.
            started("old-child", Some("old-root")),
            phase("old-child", AgentRunPhase::Running, terminal),
        ]);
        assert_status(&mut harness, AgentRunPhase::Running);
        harness.state_mut().apply_events([phase(
            "root",
            AgentRunPhase::Running,
            AgentRunPhase::Waiting,
        )]);
        assert_status(&mut harness, AgentRunPhase::Waiting);
    }
}

#[test]
fn resumed_thread_status_is_reconstructed_from_persisted_events() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("events.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    for event in [
        started("old-root", None),
        phase("old-root", AgentRunPhase::Running, AgentRunPhase::Error),
        started("root", None),
        started("child", Some("root")),
        phase("child", AgentRunPhase::Running, AgentRunPhase::Error),
        phase("root", AgentRunPhase::Running, AgentRunPhase::Error),
        started("root", None),
        phase("root", AgentRunPhase::Pending, AgentRunPhase::Pending),
        phase("root", AgentRunPhase::Pending, AgentRunPhase::Running),
        phase("root", AgentRunPhase::Running, AgentRunPhase::Waiting),
    ] {
        storage.handle().append_event(Some("gui"), &event).unwrap();
    }
    storage.close();
    let db = Database::open(&config).unwrap();
    let mut restored = state(dir.path());
    // Replay is idempotent, even without a saved sidebar run index.
    for _ in 0..2 {
        restored.restore_history(&db).unwrap();
    }
    let mut harness = HeadlessWorkbench::new(restored, [1200.0, 900.0]);
    assert_status(&mut harness, AgentRunPhase::Waiting);
    assert_eq!(
        harness.state().sidebar().threads[0].run_ids,
        ["old-root", "root", "child"]
    );
}
