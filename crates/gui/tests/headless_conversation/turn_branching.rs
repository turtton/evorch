use event_bus::{AgentRunPhase, Event, LifecycleEvent, MessageEvent};
use gui::model::commands::WorkbenchCommand;
use gui::model::composer::ProviderStatus;
use gui::model::transcript::{TranscriptEntry, TranscriptModel};
use gui::{app::WorkbenchState, fixture::DemoSource, headless::HeadlessWorkbench};
use storage::{Database, Storage, StorageConfig};
use workspace_ui::{ProjectId, SidebarState, ThreadId, ThreadRecord, UiSettings};

fn state(root: &std::path::Path) -> WorkbenchState<DemoSource> {
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("project");
    sidebar
        .add_project(project.clone(), "project", root)
        .unwrap();
    sidebar.select_project(&project).unwrap();
    sidebar
        .create_thread(ThreadId::new("one"), project, "Topic")
        .unwrap();
    sidebar.switch_thread(&ThreadId::new("one")).unwrap();
    WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar)
        .with_sidebar_path(root.join("sidebar.json"))
        .with_provider_status(ProviderStatus::Configured)
}

/// Persists events like the storage bridge and folds them into the workbench.
struct Conversation {
    storage: Storage,
    config: StorageConfig,
}

impl Conversation {
    fn new(root: &std::path::Path) -> Self {
        let config = StorageConfig {
            db_path: root.join("events.db"),
            ..Default::default()
        };
        Self {
            storage: Storage::open(config.clone()).unwrap(),
            config,
        }
    }

    fn apply(&self, gui: &mut WorkbenchState<DemoSource>, events: Vec<Event>) {
        for event in events {
            self.storage
                .handle()
                .append_event(Some("evorch-gui"), &event)
                .unwrap();
            gui.apply_events([event]);
        }
    }

    /// One user message and a completed reply on the active thread's chat run.
    /// The run starts with `context_len` 2 (fresh) or `start` (seeded).
    fn turn(
        &self,
        gui: &mut WorkbenchState<DemoSource>,
        (thread, run, start): (&str, &str, bool),
        (prompt, reply): (&str, &str),
        context_len: u64,
    ) {
        gui.composer_mut().input = prompt.into();
        gui.submit_composer();
        let mut events = Vec::new();
        if start {
            events.push(Event::new(LifecycleEvent::AgentRunStarted {
                run_id: run.into(),
                parent_run_id: None,
                agent_name: format!("chat:Worker:{thread}"),
                role: "worker".into(),
            }));
        }
        let from = if start {
            AgentRunPhase::Pending
        } else {
            AgentRunPhase::Waiting
        };
        events.extend([
            phase(run, from, AgentRunPhase::Running),
            Event::new(MessageEvent::MessageCompleted {
                run_id: run.into(),
                text: reply.into(),
            }),
            phase(run, AgentRunPhase::Running, AgentRunPhase::Waiting),
            Event::new(LifecycleEvent::TurnCompleted {
                run_id: run.into(),
                context_len,
            }),
        ]);
        self.apply(gui, events);
    }
}

fn phase(run: &str, from: AgentRunPhase, to: AgentRunPhase) -> Event {
    Event::new(LifecycleEvent::AgentRunStateChanged {
        run_id: run.into(),
        from,
        to,
        reason: None,
    })
}

/// Two completed turns on thread "one".
fn two_turns(root: &std::path::Path) -> (WorkbenchState<DemoSource>, Conversation) {
    let conversation = Conversation::new(root);
    let mut gui = state(root);
    conversation.turn(&mut gui, ("one", "run-1", true), ("first", "answer one"), 2);
    conversation.turn(
        &mut gui,
        ("one", "run-1", false),
        ("second", "answer two"),
        4,
    );
    (gui, conversation)
}

fn texts(model: &TranscriptModel) -> Vec<String> {
    model
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::UserMessage { text } | TranscriptEntry::Message { text, .. } => {
                Some(text.clone())
            }
            TranscriptEntry::Branch { .. } => Some("<branch>".into()),
            _ => None,
        })
        .collect()
}

/// Absolute id of the first completed turn in the active transcript.
fn first_turn(gui: &WorkbenchState<DemoSource>) -> usize {
    gui.transcript()
        .entries()
        .iter()
        .position(|entry| matches!(entry, TranscriptEntry::TurnEnd { .. }))
        .unwrap()
}

fn user_message(gui: &WorkbenchState<DemoSource>, text: &str) -> usize {
    gui.transcript()
        .entries()
        .iter()
        .position(|entry| matches!(entry, TranscriptEntry::UserMessage { text: t } if t == text))
        .unwrap()
}

fn visible_rows(gui: &WorkbenchState<DemoSource>) -> Vec<String> {
    let mut rows: Vec<_> =
        ThreadRecord::partition_for_project(&gui.sidebar().threads, &ProjectId::new("project"))
            .0
            .iter()
            .map(|thread| thread.id.to_string())
            .collect();
    rows.sort();
    rows
}

fn last_seed(gui: &WorkbenchState<DemoSource>) -> Option<runtime::ChatForkSeed> {
    gui.issued()
        .iter()
        .rev()
        .find_map(|command| match command {
            WorkbenchCommand::SendChat(chat) => Some(chat.fork_seed.clone()),
            _ => None,
        })?
}

#[test]
fn fork_from_a_turn_inherits_history_up_to_it_and_seeds_the_next_message() {
    // Given: a thread with two completed turns, each offering a fork action.
    let temp = tempfile::tempdir().unwrap();
    let (gui, _conversation) = two_turns(temp.path());
    let mut harness = HeadlessWorkbench::new(gui, [1600.0, 900.0]);
    harness.run();
    assert_eq!(harness.count_labels("ここから fork"), 2);
    // When: forking from the first turn.
    let turn = first_turn(harness.state());
    let fork = harness
        .state_mut()
        .fork_at_turn(ThreadId::new("one"), turn)
        .unwrap();
    // Then: the fork shows only the first turn and stays a separate child row.
    let gui = harness.state_mut();
    assert_eq!(gui.sidebar().active_thread.as_ref(), Some(&fork));
    assert_eq!(texts(gui.transcript()), ["first", "answer one", "<branch>"]);
    assert_eq!(visible_rows(gui), ["one".to_string(), fork.to_string()]);
    // And: its first message starts the model from the source turn's saved context.
    gui.composer_mut().input = "branch".into();
    gui.submit_composer();
    assert_eq!(
        last_seed(gui),
        Some(runtime::ChatForkSeed {
            source_run_id: "run-1".into(),
            context_len: 2,
        })
    );
}

#[test]
fn rewind_replaces_the_row_in_place_and_can_be_undone() {
    // Given: a thread with two completed turns.
    let temp = tempfile::tempdir().unwrap();
    let (mut gui, _conversation) = two_turns(temp.path());
    let turn = first_turn(&gui);
    // When: rewinding to the first turn.
    let version = gui.rewind_to_turn(ThreadId::new("one"), turn).unwrap();
    // Then: the same single row now shows the conversation as of turn one.
    assert_eq!(visible_rows(&gui), [version.to_string()]);
    assert_eq!(texts(gui.transcript()), ["first", "answer one", "<branch>"]);
    let saved = workspace_ui::load_sidebar(&temp.path().join("sidebar.json")).unwrap();
    assert!(
        saved
            .threads
            .iter()
            .any(|thread| thread.id == ThreadId::new("one") && thread.superseded)
    );
    let mut harness = HeadlessWorkbench::new(gui, [1600.0, 900.0]);
    harness.run();
    assert!(harness.has_label("版 2 / 2"));
    // When: restoring the pre-rewind version from the branch marker.
    harness.click_label("巻き戻し前に戻す");
    harness.run();
    // Then: the original conversation is back, still offering the newer version.
    let gui = harness.state();
    assert_eq!(
        gui.sidebar().active_thread.as_ref(),
        Some(&ThreadId::new("one"))
    );
    assert_eq!(visible_rows(gui), ["one"]);
    assert_eq!(
        texts(gui.transcript()),
        ["first", "answer one", "second", "answer two"]
    );
    assert!(harness.has_label("版 1 / 2"));
}

#[test]
fn edit_from_a_message_rewinds_before_it_and_returns_it_to_the_composer() {
    // Given: a thread with two completed turns.
    let temp = tempfile::tempdir().unwrap();
    let (mut gui, _conversation) = two_turns(temp.path());
    let message = user_message(&gui, "second");
    // When: editing the second turn's message.
    gui.edit_from_message(ThreadId::new("one"), message)
        .unwrap();
    // Then: history ends at the first turn and the message is ready to resend.
    assert_eq!(texts(gui.transcript()), ["first", "answer one", "<branch>"]);
    assert_eq!(gui.composer().input, "second");
    gui.submit_composer();
    assert_eq!(
        last_seed(&gui),
        Some(runtime::ChatForkSeed {
            source_run_id: "run-1".into(),
            context_len: 2,
        })
    );
    // And: editing the very first message branches from the start, with no seed.
    let other = tempfile::tempdir().unwrap();
    let mut fresh = two_turns(other.path()).0;
    let first = user_message(&fresh, "first");
    fresh
        .edit_from_message(ThreadId::new("one"), first)
        .unwrap();
    assert_eq!(texts(fresh.transcript()), ["<branch>"]);
    fresh.submit_composer();
    assert_eq!(last_seed(&fresh), None);
}

#[test]
fn rewind_is_refused_while_a_turn_is_unfinished() {
    // Given: a completed turn followed by a mid-turn wait on the same run.
    let temp = tempfile::tempdir().unwrap();
    let conversation = Conversation::new(temp.path());
    let mut gui = state(temp.path());
    conversation.turn(&mut gui, ("one", "run-1", true), ("first", "answer one"), 2);
    let turn = first_turn(&gui);
    conversation.apply(
        &mut gui,
        vec![
            phase("run-1", AgentRunPhase::Waiting, AgentRunPhase::Running),
            phase("run-1", AgentRunPhase::Running, AgentRunPhase::Waiting),
        ],
    );
    // When/Then: rewinding is refused, but forking the completed turn still works.
    assert!(gui.rewind_to_turn(ThreadId::new("one"), turn).is_err());
    assert_eq!(visible_rows(&gui), ["one"]);
    assert!(gui.fork_at_turn(ThreadId::new("one"), turn).is_ok());
}

#[test]
fn restored_history_rebuilds_forks_and_versions() {
    // Given: a fork that continued on its own run, and a rewound version of the source.
    let temp = tempfile::tempdir().unwrap();
    let (mut gui, conversation) = two_turns(temp.path());
    let turn = first_turn(&gui);
    let fork = gui.fork_at_turn(ThreadId::new("one"), turn).unwrap();
    conversation.turn(
        &mut gui,
        (&fork.to_string(), "run-2", true),
        ("branch", "answer branch"),
        4,
    );
    gui.switch_thread(ThreadId::new("one")).unwrap();
    let version = gui.rewind_to_turn(ThreadId::new("one"), turn).unwrap();
    gui.save_sidebar();
    drop(gui);
    // When: a fresh workbench replays persisted history.
    let sidebar = workspace_ui::load_sidebar(&temp.path().join("sidebar.json")).unwrap();
    let mut reopened = state(temp.path()).with_sidebar(sidebar);
    reopened
        .restore_history(&Database::open(&conversation.config).unwrap())
        .unwrap();
    // Then: each branch shows inherited history up to its turn and its own continuation.
    reopened.switch_thread(fork.clone()).unwrap();
    assert_eq!(
        texts(reopened.transcript()),
        ["first", "answer one", "<branch>", "branch", "answer branch"]
    );
    reopened.switch_thread(ThreadId::new("one")).unwrap();
    assert_eq!(reopened.sidebar().active_thread.as_ref(), Some(&version));
    assert_eq!(
        texts(reopened.transcript()),
        ["first", "answer one", "<branch>"]
    );
    reopened.switch_version(ThreadId::new("one")).unwrap();
    assert_eq!(
        texts(reopened.transcript()),
        ["first", "answer one", "second", "answer two"]
    );
}

#[test]
fn completed_turn_marks_its_thread_updated_and_persists_the_time() {
    // Given: thread "one" with a run, and a newer idle thread "two".
    let temp = tempfile::tempdir().unwrap();
    let conversation = Conversation::new(temp.path());
    let mut gui = state(temp.path());
    let project = ProjectId::new("project");
    let mut sidebar = gui.sidebar().clone();
    sidebar
        .create_thread(ThreadId::new("two"), project.clone(), "Later")
        .unwrap();
    for thread in &mut sidebar.threads {
        thread.created_at = if thread.id == ThreadId::new("one") {
            10
        } else {
            20
        };
        thread.updated_at = 0;
    }
    gui = gui.with_sidebar(sidebar);
    conversation.apply(
        &mut gui,
        vec![Event::new(LifecycleEvent::AgentRunStarted {
            run_id: "run-1".into(),
            parent_run_id: None,
            agent_name: "chat:Worker:one".into(),
            role: "worker".into(),
        })],
    );
    // When: the older thread's run completes a turn at a known time.
    let mut turn = Event::new(LifecycleEvent::TurnCompleted {
        run_id: "run-1".into(),
        context_len: 2,
    });
    turn.meta.wall_clock = std::time::UNIX_EPOCH + std::time::Duration::from_secs(4_000_000_000);
    conversation.apply(&mut gui, vec![turn]);
    // Then: the saved activity time moves it above the newer thread.
    let saved = workspace_ui::load_sidebar(&temp.path().join("sidebar.json")).unwrap();
    let one = saved
        .threads
        .iter()
        .find(|thread| thread.id == ThreadId::new("one"))
        .unwrap();
    assert_eq!(one.updated_at, 4_000_000_000);
    let (main, _) = ThreadRecord::partition_for_project(&saved.threads, &project);
    assert_eq!(
        main.iter()
            .map(|thread| thread.title.as_str())
            .collect::<Vec<_>>(),
        ["Topic", "Later"]
    );
}
