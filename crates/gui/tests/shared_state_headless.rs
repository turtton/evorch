//! Two GUI processes sharing one event database, one `sidebar.json` and one
//! ownership registry: what one does shows up in the other, and neither
//! overwrites the other's work.
//!
//! Each window has its own storage connection and event bus, as two processes
//! would. The window "persists" events through its own stream exactly as the
//! recorder does, and the follower is polled synchronously.

use std::collections::BTreeSet;
use std::sync::Arc;

use event_bus::{
    AgentRunPhase, Event, EventBus, LifecycleEvent, MessageEvent, OwnershipAction, OwnershipEvent,
};
use gui::app::WorkbenchState;
use gui::fixture::DemoSource;
use gui::model::composer::ProviderStatus;
use gui::model::transcript::TranscriptEntry;
use runtime::ownership::OwnerHost;
use storage::{Database, Storage, StorageConfig};
use tempfile::TempDir;
use workspace_ui::{ThreadId, ThreadRunPhase, ThreadState, UiSettings};

struct Shared {
    dir: TempDir,
    project: TempDir,
    config: StorageConfig,
}

impl Shared {
    fn new() -> Self {
        let dir = TempDir::new().expect("temp dir");
        let config = StorageConfig {
            db_path: dir.path().join("events.db"),
            ..StorageConfig::default()
        };
        Self {
            dir,
            project: TempDir::new().expect("project dir"),
            config,
        }
    }

    fn sidebar(&self) -> std::path::PathBuf {
        self.dir.path().join("sidebar.json")
    }
}

struct Window {
    state: WorkbenchState<DemoSource>,
    storage: Storage,
    stream: String,
    host: Arc<OwnerHost>,
}

fn open_window(shared: &Shared, stream: &str) -> Window {
    open_window_with_sidebar(shared, stream, shared.sidebar())
}

fn open_window_with_sidebar(
    shared: &Shared,
    stream: &str,
    sidebar_path: std::path::PathBuf,
) -> Window {
    let storage = Storage::open(shared.config.clone()).expect("storage must open");
    let bus = Arc::new(EventBus::new(64));
    let host = Arc::new(
        OwnerHost::open(&shared.dir.path().join("threads"), Default::default(), bus)
            .expect("owner host must open"),
    );
    let mut state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .expect("workbench")
        .with_provider_status(ProviderStatus::Configured)
        .with_ownership(Arc::clone(&host))
        .with_sidebar_path(sidebar_path.clone());
    if sidebar_path.exists() {
        state = state.with_sidebar(workspace_ui::load_sidebar(&sidebar_path).expect("sidebar"));
    }
    state
        .restore_history(&Database::open(&shared.config).expect("database"))
        .expect("restore");
    let state = state
        .with_manual_shared_feed(shared.config.db_path.clone(), stream.into())
        .expect("feed");
    Window {
        state,
        storage,
        stream: stream.into(),
        host,
    }
}

impl Window {
    /// What the recorder and the live pump do for one of this window's own events.
    fn emit(&mut self, event: Event) {
        self.storage
            .handle()
            .append_stream_event(&self.stream, &event, None)
            .expect("event must persist");
        self.state.apply_events([event]);
    }

    fn open_thread(&mut self, shared: &Shared, title: &str) -> ThreadId {
        let project = self
            .state
            .add_project(shared.project.path())
            .expect("project");
        self.state.select_project(project).expect("select");
        self.state.create_thread(title).expect("thread")
    }
}

fn run_started(run: &str, thread: &ThreadId) -> Event {
    Event::new(LifecycleEvent::AgentRunStarted {
        run_id: run.into(),
        parent_run_id: None,
        agent_name: format!("chat:Worker:{thread}"),
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

fn reply(run: &str, text: &str) -> Event {
    Event::new(MessageEvent::MessageCompleted {
        run_id: run.into(),
        text: text.into(),
    })
}

fn texts(state: &WorkbenchState<DemoSource>) -> Vec<String> {
    state
        .transcript()
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::UserMessage { text } | TranscriptEntry::Message { text, .. } => {
                Some(text.clone())
            }
            _ => None,
        })
        .collect()
}

fn thread_state(state: &WorkbenchState<DemoSource>, thread: &ThreadId) -> ThreadState {
    state
        .sidebar()
        .threads
        .iter()
        .find(|record| &record.id == thread)
        .expect("thread")
        .state(state.thread_phases())
}

fn event_count(shared: &Shared) -> i64 {
    Database::open_read_only(&shared.config.db_path)
        .expect("read-only open")
        .last_event_id()
        .expect("last id")
}

/// Window A owns a thread and has a running conversation in it; window B has the thread open.
fn working_owner_and_open_follower(shared: &Shared) -> (Window, Window, ThreadId) {
    let mut a = open_window(shared, "evorch-gui:a");
    let thread = a.open_thread(shared, "Alpha");
    a.host
        .start(&thread.to_string())
        .expect("A owns the thread");
    let mut b = open_window(shared, "evorch-gui:b");
    b.state.switch_thread(thread.clone()).expect("B opens it");

    a.state.composer_mut().input = "hello from a".into();
    a.state.submit_composer();
    a.emit(run_started("run-a", &thread));
    a.emit(phase(
        "run-a",
        AgentRunPhase::Pending,
        AgentRunPhase::Running,
    ));
    a.emit(reply("run-a", "reply from a"));
    (a, b, thread)
}

#[test]
fn a_thread_worked_on_elsewhere_shows_its_transcript_and_phase_and_rejects_input() {
    // Given: window A owns a running conversation, window B has the thread open
    let shared = Shared::new();
    let (a, mut b, thread) = working_owner_and_open_follower(&shared);
    let before = event_count(&shared);

    // When: B follows the shared log
    let arrived = b.state.poll_shared_feed();

    // Then: B shows A's conversation and that it is running
    assert!(arrived >= 3, "arrived {arrived}");
    assert_eq!(texts(&b.state), ["hello from a", "reply from a"]);
    assert_eq!(
        b.state.thread_phases().get("run-a"),
        Some(&ThreadRunPhase::Running)
    );
    assert_eq!(thread_state(&b.state, &thread), ThreadState::Running);

    // And: only A may write; B's fold persisted nothing and emitted nothing
    assert!(a.state.thread_writable());
    assert!(!b.state.thread_writable());
    assert_eq!(
        event_count(&shared),
        before,
        "following must not re-persist"
    );
    assert_eq!(b.state.poll_shared_feed(), 0);
}

#[test]
fn a_window_does_not_fold_its_own_events_a_second_time() {
    // Given: window A persisted and folded its conversation
    let shared = Shared::new();
    let (mut a, _b, _thread) = working_owner_and_open_follower(&shared);

    // When: A follows the log, which holds only its own rows
    let arrived = a.state.poll_shared_feed();

    // Then: nothing arrives and the transcript is not duplicated
    assert_eq!(arrived, 0);
    assert_eq!(texts(&a.state), ["hello from a", "reply from a"]);
}

#[test]
fn a_restart_replays_history_once_and_following_starts_after_it() {
    // Given: window A's conversation is in the log
    let shared = Shared::new();
    let (_a, _b, thread) = working_owner_and_open_follower(&shared);

    // When: a new window starts (restore replays) and then follows
    let mut c = open_window(&shared, "evorch-gui:c");
    c.state.switch_thread(thread).expect("open");
    let replayed = texts(&c.state);
    let arrived = c.state.poll_shared_feed();

    // Then: the replay holds the conversation once and following adds nothing
    assert_eq!(replayed, ["hello from a", "reply from a"]);
    assert_eq!(arrived, 0);
    assert_eq!(texts(&c.state), replayed);
}

#[test]
fn a_released_thread_stops_showing_its_runs_as_active() {
    // Given: B shows A's run as running
    let shared = Shared::new();
    let (mut a, mut b, thread) = working_owner_and_open_follower(&shared);
    b.state.poll_shared_feed();
    assert_eq!(thread_state(&b.state, &thread), ThreadState::Running);

    // When: A exits and releases the thread without a final phase event
    a.emit(Event::new(OwnershipEvent {
        thread_id: thread.to_string(),
        owner_id: a.host.owner_id().to_owned(),
        generation: 1,
        action: OwnershipAction::Released,
    }));
    b.state.poll_shared_feed();

    // Then: B shows it as stopped, not running forever
    assert_eq!(
        b.state.thread_phases().get("run-a"),
        Some(&ThreadRunPhase::Stopped)
    );
    assert_eq!(thread_state(&b.state, &thread), ThreadState::Stopped);
}

#[test]
fn a_late_final_event_still_settles_a_run_that_ended_normally() {
    // Given: A finishes its run and then releases the thread
    let shared = Shared::new();
    let (mut a, mut b, thread) = working_owner_and_open_follower(&shared);
    a.emit(phase("run-a", AgentRunPhase::Running, AgentRunPhase::Done));
    a.emit(Event::new(OwnershipEvent {
        thread_id: thread.to_string(),
        owner_id: a.host.owner_id().to_owned(),
        generation: 1,
        action: OwnershipAction::Released,
    }));

    // When: B follows both in one batch
    b.state.poll_shared_feed();

    // Then: the recorded Done is kept, not overwritten by the release
    assert_eq!(
        b.state.thread_phases().get("run-a"),
        Some(&ThreadRunPhase::Done)
    );
}

#[test]
fn sidebar_changes_of_both_windows_reach_both_and_neither_overwrites_the_other() {
    // Given: both windows hold the same thread
    let shared = Shared::new();
    let mut a = open_window(&shared, "evorch-gui:a");
    let alpha = a.open_thread(&shared, "Alpha");
    let mut b = open_window(&shared, "evorch-gui:b");

    // When: A renames it while B, unaware, creates another thread
    a.state
        .rename_thread(alpha.clone(), "Renamed by A".into())
        .expect("rename");
    let from_b = b.state.create_thread("From B").expect("create");
    let on_disk = workspace_ui::load_sidebar(&shared.sidebar()).expect("sidebar");

    // Then: the saved file holds both changes
    let titles: BTreeSet<_> = on_disk.threads.iter().map(|t| t.title.clone()).collect();
    assert_eq!(
        titles,
        BTreeSet::from(["Renamed by A".to_owned(), "From B".to_owned()])
    );

    // And: following brings each window up to date without moving its selection
    a.state.poll_shared_feed();
    b.state.poll_shared_feed();
    for window in [&a, &b] {
        let titles: BTreeSet<_> = window
            .state
            .sidebar()
            .threads
            .iter()
            .map(|t| t.title.clone())
            .collect();
        assert_eq!(titles.len(), 2, "{titles:?}");
        assert!(titles.contains("Renamed by A") && titles.contains("From B"));
    }
    assert_eq!(a.state.sidebar().active_thread, Some(alpha));
    assert_eq!(b.state.sidebar().active_thread, Some(from_b));
}

#[test]
fn threads_created_by_both_windows_get_distinct_ids() {
    // Given: two windows creating threads without seeing each other's
    let shared = Shared::new();
    let mut a = open_window(&shared, "evorch-gui:a");
    a.open_thread(&shared, "seed");
    let mut b = open_window(&shared, "evorch-gui:b");

    // When: each creates 20 threads, then both follow
    let mut created = BTreeSet::new();
    for index in 0..20 {
        created.insert(a.state.create_thread(format!("a{index}")).expect("a"));
        created.insert(b.state.create_thread(format!("b{index}")).expect("b"));
    }
    a.state.poll_shared_feed();
    b.state.poll_shared_feed();

    // Then: no id was reused and both windows hold all of them
    assert_eq!(created.len(), 40);
    for window in [&a, &b] {
        let known: BTreeSet<_> = window
            .state
            .sidebar()
            .threads
            .iter()
            .map(|t| t.id.clone())
            .collect();
        assert!(created.is_subset(&known));
        assert_eq!(known.len(), 41);
    }
}

#[test]
fn events_of_a_conversation_the_sidebar_lacks_wait_for_the_sidebar() {
    // Given: A works in a thread whose sidebar B cannot see yet (A saves to a private file)
    let shared = Shared::new();
    let private = shared.dir.path().join("private.json");
    let mut a = open_window_with_sidebar(&shared, "evorch-gui:a", private.clone());
    let mut b = open_window(&shared, "evorch-gui:b");
    let thread = a.open_thread(&shared, "Alpha");
    a.emit(run_started("run-a", &thread));
    a.emit(phase(
        "run-a",
        AgentRunPhase::Pending,
        AgentRunPhase::Running,
    ));
    a.emit(reply("run-a", "early reply"));

    // When: B follows before the sidebar names the thread
    b.state.poll_shared_feed();

    // Then: the chat start is held back rather than lost
    assert!(b.state.thread_phases().is_empty());

    // When: the sidebar appears
    std::fs::copy(&private, shared.sidebar()).expect("publish sidebar");
    b.state.poll_shared_feed();
    b.state.switch_thread(thread).expect("open");

    // Then: the held events are shown in order
    assert_eq!(texts(&b.state), ["early reply"]);
    assert_eq!(
        b.state.thread_phases().get("run-a"),
        Some(&ThreadRunPhase::Running)
    );
}
