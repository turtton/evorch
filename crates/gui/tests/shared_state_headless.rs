//! Two GUI processes sharing one event database, one `sidebar.json` and one
//! ownership registry: what one does shows up in the other, and neither
//! overwrites the other's work.
//!
//! Each window has its own storage connection and event bus, as two processes
//! would. The window "persists" events through its own stream exactly as the
//! recorder does, and the follower is polled synchronously.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use event_bus::{
    AgentRunPhase, Event, EventBus, LifecycleEvent, MessageEvent, OwnershipAction, OwnershipEvent,
    ToolEvent,
};
use gui::app::WorkbenchState;
use gui::fixture::DemoSource;
use gui::model::commands::{CommandSink, LoopEvent, WorkbenchCommand};
use gui::model::composer::ProviderStatus;
use gui::model::system_notifications::{SystemNotification, SystemNotificationSink};
use gui::model::transcript::TranscriptEntry;
use recorder::storage_bridge::{OwnedStorageBridge, StorageBridge};
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
    bus: Arc<EventBus>,
}

fn open_window(shared: &Shared, stream: &str) -> Window {
    open_window_with_sidebar(shared, stream, shared.sidebar())
}

fn open_window_with_sidebar(
    shared: &Shared,
    stream: &str,
    sidebar_path: std::path::PathBuf,
) -> Window {
    open_window_customized(shared, stream, sidebar_path, |state| state)
}

fn open_window_customized(
    shared: &Shared,
    stream: &str,
    sidebar_path: std::path::PathBuf,
    customize: impl FnOnce(WorkbenchState<DemoSource>) -> WorkbenchState<DemoSource>,
) -> Window {
    let storage = Storage::open(shared.config.clone()).expect("storage must open");
    let bus = Arc::new(EventBus::new(64));
    let host = Arc::new(
        OwnerHost::open(
            &shared.dir.path().join("threads"),
            Default::default(),
            Arc::clone(&bus),
        )
        .expect("owner host must open"),
    );
    let mut state = customize(
        WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
            .expect("workbench")
            .with_provider_status(ProviderStatus::Configured)
            .with_ownership(Arc::clone(&host)),
    )
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
        bus,
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

fn turn_completed(run: &str, context_len: u64) -> Event {
    Event::new(LifecycleEvent::TurnCompleted {
        run_id: run.into(),
        context_len,
    })
}

fn ownership(thread: &ThreadId, owner: &str, generation: u64, action: OwnershipAction) -> Event {
    Event::new(OwnershipEvent {
        thread_id: thread.to_string(),
        owner_id: owner.into(),
        generation,
        action,
    })
}

fn titles(state: &WorkbenchState<DemoSource>) -> BTreeSet<String> {
    state
        .sidebar()
        .threads
        .iter()
        .map(|thread| thread.title.clone())
        .collect()
}

fn lock_file(shared: &Shared) -> std::fs::File {
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(shared.dir.path().join("sidebar.json.lock"))
        .expect("lock file")
}

#[derive(Default)]
struct Counters {
    submitted: AtomicUsize,
    observed: AtomicUsize,
}

struct CountingSink(Arc<Counters>);

impl CommandSink for CountingSink {
    fn observe_lifecycle(&mut self, _event: &Event) {
        self.0.observed.fetch_add(1, Ordering::SeqCst);
    }

    fn submit(&mut self, _command: WorkbenchCommand) -> Vec<LoopEvent> {
        self.0.submitted.fetch_add(1, Ordering::SeqCst);
        Vec::new()
    }
}

#[derive(Default)]
struct Notifications(Mutex<Vec<SystemNotification>>);

impl SystemNotificationSink for Notifications {
    fn send(&self, notification: SystemNotification) {
        self.0.lock().expect("notifications").push(notification);
    }
}

/// One logic pass of an unfocused window, which is when notifications are delivered.
fn unfocused_logic(state: &mut WorkbenchState<DemoSource>) {
    let ctx = egui::Context::default();
    let mut input = egui::RawInput {
        focused: false,
        ..Default::default()
    };
    input
        .viewports
        .get_mut(&egui::ViewportId::ROOT)
        .expect("root viewport")
        .focused = Some(false);
    let _ = ctx.run_logic(&input, |ctx| state.logic(ctx));
}

#[test]
fn a_save_that_finds_the_file_locked_is_deferred_and_repeated_without_losing_either_side() {
    // Given: both windows hold the thread, and another process holds the sidebar lock
    let shared = Shared::new();
    let mut a = open_window(&shared, "evorch-gui:a");
    let alpha = a.open_thread(&shared, "Alpha");
    let mut b = open_window(&shared, "evorch-gui:b");
    let held = lock_file(&shared);
    held.lock().expect("hold the lock");

    // When: A renames the thread
    a.state
        .rename_thread(alpha, "Renamed by A".into())
        .expect("rename");

    // Then: nothing was written without the lock; the save waits
    let on_disk = workspace_ui::load_sidebar(&shared.sidebar()).expect("sidebar");
    assert_eq!(on_disk.threads[0].title, "Alpha");
    assert!(a.state.sidebar_save_deferred());

    // When: the lock is released, B saves its own change, and A repeats its save
    drop(held);
    b.state.create_thread("From B").expect("create");
    a.state.flush_deferred_sidebar_save();

    // Then: the file holds both changes and nothing is pending
    assert!(!a.state.sidebar_save_deferred());
    let on_disk = workspace_ui::load_sidebar(&shared.sidebar()).expect("sidebar");
    let on_disk: BTreeSet<_> = on_disk.threads.into_iter().map(|t| t.title).collect();
    assert_eq!(
        on_disk,
        BTreeSet::from(["Renamed by A".to_owned(), "From B".to_owned()])
    );
}

#[test]
fn a_save_by_another_process_between_the_first_load_and_the_restore_survives() {
    // Given: A saved a thread; B loaded that sidebar the way startup does
    let shared = Shared::new();
    let mut a = open_window(&shared, "evorch-gui:a");
    let alpha = a.open_thread(&shared, "Alpha");
    let loaded = workspace_ui::load_sidebar(&shared.sidebar()).expect("sidebar");
    let state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .expect("workbench")
        .with_provider_status(ProviderStatus::Configured)
        .with_sidebar(loaded)
        .with_sidebar_path(shared.sidebar());

    // When: A saves another thread before B restores, and B then saves
    a.state.create_thread("Beta").expect("create");
    let mut state = state;
    state
        .restore_history(&Database::open(&shared.config).expect("database"))
        .expect("restore");
    state.toggle_pin(alpha).expect("pin");
    state.save_sidebar();

    // Then: B's save did not erase A's thread
    let on_disk = workspace_ui::load_sidebar(&shared.sidebar()).expect("sidebar");
    let on_disk: BTreeSet<_> = on_disk.threads.iter().map(|t| t.title.clone()).collect();
    assert_eq!(
        on_disk,
        BTreeSet::from(["Alpha".to_owned(), "Beta".to_owned()])
    );
    assert!(
        workspace_ui::load_sidebar(&shared.sidebar())
            .expect("sidebar")
            .threads
            .iter()
            .any(|thread| thread.title == "Alpha" && thread.pinned)
    );
}

#[test]
fn a_release_stops_only_the_runs_active_when_it_happened() {
    // Given: C observes A's running conversation
    let shared = Shared::new();
    let (a, _b, thread) = working_owner_and_open_follower(&shared);
    let mut c = open_window(&shared, "evorch-gui:c");
    c.state.poll_shared_feed();
    assert_eq!(
        c.state.thread_phases().get("run-a"),
        Some(&ThreadRunPhase::Running)
    );

    // When: A releases, D claims at the next generation and starts a new run, all seen in one poll
    let persist = |stream: &str, event: Event| {
        a.storage
            .handle()
            .append_stream_event(stream, &event, None)
            .expect("event must persist");
    };
    persist(
        "evorch-gui:a",
        ownership(&thread, "owner-a", 1, OwnershipAction::Released),
    );
    persist(
        "evorch-gui:d",
        ownership(&thread, "owner-d", 2, OwnershipAction::Claimed),
    );
    persist("evorch-gui:d", run_started("run-new", &thread));
    persist(
        "evorch-gui:d",
        phase("run-new", AgentRunPhase::Pending, AgentRunPhase::Running),
    );
    c.state.poll_shared_feed();

    // Then: the old run ended with the release, the new owner's run keeps running
    assert_eq!(
        c.state.thread_phases().get("run-a"),
        Some(&ThreadRunPhase::Stopped)
    );
    assert_eq!(
        c.state.thread_phases().get("run-new"),
        Some(&ThreadRunPhase::Running)
    );
    assert_eq!(thread_state(&c.state, &thread), ThreadState::Running);
}

#[test]
fn a_sidebar_published_after_a_restart_recovers_the_conversations_earlier_events() {
    // Given: A completed a run in a thread whose sidebar B cannot see, and B restarted meanwhile
    let shared = Shared::new();
    let private = shared.dir.path().join("private.json");
    let mut a = open_window_with_sidebar(&shared, "evorch-gui:a", private.clone());
    let thread = a.open_thread(&shared, "Alpha");
    a.state.composer_mut().input = "question".into();
    a.state.submit_composer();
    a.emit(run_started("run-a", &thread));
    a.emit(phase(
        "run-a",
        AgentRunPhase::Pending,
        AgentRunPhase::Running,
    ));
    a.emit(reply("run-a", "answer"));
    a.emit(phase("run-a", AgentRunPhase::Running, AgentRunPhase::Done));
    let mut b = open_window(&shared, "evorch-gui:b");
    assert!(
        b.state.sidebar().threads.is_empty(),
        "the restore replayed events of a conversation it does not know"
    );

    // When: the sidebar naming the thread is published and B follows
    std::fs::copy(&private, shared.sidebar()).expect("publish sidebar");
    b.state.poll_shared_feed();
    b.state.switch_thread(thread).expect("open");

    // Then: the conversation shows what happened before it was known, once and in order
    assert_eq!(texts(&b.state), ["question", "answer"]);
}

#[test]
fn a_followed_run_that_was_restored_as_active_keeps_following_fast() {
    // Given: A's run is running when B starts
    let shared = Shared::new();
    let (_a, _b, _thread) = working_owner_and_open_follower(&shared);

    // When: B restores
    let b = open_window(&shared, "evorch-gui:b2");

    // Then: following treats it as active work
    assert!(b.state.foreign_work_active());
}

#[test]
fn what_other_windows_do_is_shown_without_acting_on_it() {
    // Given: B counts the commands and lifecycle observations it receives and the OS
    // notifications it sends, and has its own recorder on its own bus
    let shared = Shared::new();
    let counters = Arc::new(Counters::default());
    let notifications = Arc::new(Notifications::default());
    let mut a = open_window(&shared, "evorch-gui:a");
    let thread = a.open_thread(&shared, "Alpha");
    a.host.start(&thread.to_string()).expect("A owns it");
    let mut b = open_window_customized(&shared, "evorch-gui:b", shared.sidebar(), |state| {
        state
            .with_command_sink(Box::new(CountingSink(Arc::clone(&counters))))
            .with_system_notifications(notifications.clone())
    });
    b.state.switch_thread(thread.clone()).expect("open");
    let mut recorder = OwnedStorageBridge::spawn(
        Arc::clone(&b.bus),
        Storage::open(shared.config.clone()).expect("recorder storage"),
        |handle| StorageBridge::new(handle, "evorch-gui:b"),
        Duration::from_secs(60),
    )
    .expect("recorder");
    let observed_before = counters.observed.load(Ordering::SeqCst);
    let active_before = b.state.transcript().entries().len();

    // When: A works, finishes a turn, asks for approval, wakes its root and fails something
    // that belongs to no run, and B follows
    a.emit(run_started("run-a", &thread));
    a.emit(phase(
        "run-a",
        AgentRunPhase::Pending,
        AgentRunPhase::Running,
    ));
    a.emit(Event::new(ToolEvent::ApprovalRequested {
        tool_name: "shell".into(),
        call_id: "run-a:c1".into(),
        input: None,
    }));
    a.emit(Event::new(LifecycleEvent::RootWakeRequested {
        run_id: "run-a".into(),
    }));
    a.emit(turn_completed("run-a", 2));
    a.emit(phase("run-a", AgentRunPhase::Running, AgentRunPhase::Done));
    a.emit(Event::new(LifecycleEvent::Failed {
        session_id: "elsewhere".into(),
        reason: "boom".into(),
    }));
    recorder.flush().expect("flush");
    let rows_before = event_count(&shared);
    assert!(b.state.poll_shared_feed() > 0);
    unfocused_logic(&mut b.state);
    recorder.flush().expect("flush");

    // Then: the thread's run is shown as finished
    assert_eq!(
        b.state.thread_phases().get("run-a"),
        Some(&ThreadRunPhase::Done)
    );
    // And: no command went to the runtime and no lifecycle was observed by it
    assert_eq!(counters.submitted.load(Ordering::SeqCst), 0);
    assert!(b.state.issued().is_empty());
    assert_eq!(counters.observed.load(Ordering::SeqCst), observed_before);
    // And: no approval prompt was raised and no OS notification sent
    assert_eq!(b.state.pending_approvals().items().count(), 0);
    assert!(notifications.0.lock().expect("notifications").is_empty());
    // And: the event unattributed to any run did not land in the open thread
    assert!(
        b.state.transcript().entries().len() <= active_before + 2,
        "only the run's own entries may appear"
    );
    assert!(
        !b.state
            .transcript()
            .entries()
            .iter()
            .any(|entry| matches!(entry, TranscriptEntry::Error { text } if text.contains("boom")))
    );
    // And: following persisted nothing, even with a recorder listening on this window's bus
    assert_eq!(event_count(&shared), rows_before);

    // Control: the same kinds of event applied locally do act, so the checks above can fail
    b.bus.emit(started_event("control"));
    recorder.flush().expect("flush");
    assert_eq!(
        event_count(&shared),
        rows_before + 1,
        "the recorder is live"
    );
    b.state.apply_events([
        run_started("run-b", &thread),
        Event::new(LifecycleEvent::RootWakeRequested {
            run_id: "run-b".into(),
        }),
    ]);
    assert!(counters.observed.load(Ordering::SeqCst) > observed_before);
    assert!(
        !b.state.issued().is_empty(),
        "a local wake issues a command"
    );
    b.state.apply_events([
        Event::new(ToolEvent::ToolStarted {
            tool_name: "shell".into(),
            call_id: "run-b:c2".into(),
            input: None,
            run_id: Some("run-b".into()),
            parent_call_id: None,
        }),
        Event::new(ToolEvent::ApprovalRequested {
            tool_name: "shell".into(),
            call_id: "run-b:c2".into(),
            input: None,
        }),
        phase("run-b", AgentRunPhase::Pending, AgentRunPhase::Running),
        turn_completed("run-b", 2),
        phase("run-b", AgentRunPhase::Running, AgentRunPhase::Done),
    ]);
    unfocused_logic(&mut b.state);
    assert_eq!(b.state.pending_approvals().items().count(), 1);
    assert!(!notifications.0.lock().expect("notifications").is_empty());
}

fn started_event(label: &str) -> Event {
    Event::new(LifecycleEvent::Started {
        session_id: label.into(),
    })
}

#[test]
fn importing_the_sidebar_keeps_this_windows_selection_and_typed_text() {
    // Given: B has project one selected and unsent text; A adds and selects another project
    let shared = Shared::new();
    let mut a = open_window(&shared, "evorch-gui:a");
    a.open_thread(&shared, "Alpha");
    let mut b = open_window(&shared, "evorch-gui:b");
    let selected = b.state.sidebar().selected_project.clone();
    b.state.composer_mut().input = "typing in B".into();
    let other = TempDir::new().expect("other project");
    let project = a.state.add_project(other.path()).expect("project");
    a.state.select_project(project.clone()).expect("select");
    a.state.create_thread("In two").expect("thread");

    // When: B follows
    b.state.poll_shared_feed();

    // Then: B knows the new project and thread but did not follow A's selection
    assert!(
        b.state
            .sidebar()
            .projects
            .iter()
            .any(|known| known.id == project)
    );
    assert!(titles(&b.state).contains("In two"));
    assert_eq!(b.state.sidebar().selected_project, selected);
    assert_eq!(b.state.composer().input, "typing in B");
}

#[test]
fn an_unsaved_change_survives_importing_and_is_written_by_the_next_save() {
    // Given: A's rename is deferred because another process holds the lock
    let shared = Shared::new();
    let mut a = open_window(&shared, "evorch-gui:a");
    let alpha = a.open_thread(&shared, "Alpha");
    let mut b = open_window(&shared, "evorch-gui:b");
    let held = lock_file(&shared);
    held.lock().expect("hold the lock");
    a.state
        .rename_thread(alpha, "Renamed by A".into())
        .expect("rename");
    assert!(a.state.sidebar_save_deferred());
    drop(held);

    // When: B saves, A follows (importing) without saving, B saves again, then A repeats its save
    b.state.create_thread("From B1").expect("create");
    a.state.poll_shared_feed();
    b.state.create_thread("From B2").expect("create");
    a.state.flush_deferred_sidebar_save();

    // Then: the rename reached the file next to both of B's threads, and stays after the next import
    assert!(!a.state.sidebar_save_deferred());
    let on_disk: BTreeSet<_> = workspace_ui::load_sidebar(&shared.sidebar())
        .expect("sidebar")
        .threads
        .into_iter()
        .map(|thread| thread.title)
        .collect();
    assert_eq!(
        on_disk,
        BTreeSet::from([
            "Renamed by A".to_owned(),
            "From B1".to_owned(),
            "From B2".to_owned()
        ])
    );
    a.state.poll_shared_feed();
    assert_eq!(titles(&a.state), on_disk);
}

/// A works in a thread whose sidebar B lacks: `before` is persisted, then B restarts
/// (its replay holds them, unattributed), `after` is persisted, then the sidebar is
/// published, B follows and opens the thread.
fn conversation_across_a_late_sidebar(before: &[Event], after: &[Event]) -> (Window, ThreadId) {
    let shared = Box::leak(Box::new(Shared::new()));
    let private = shared.dir.path().join("private.json");
    let mut a = open_window_with_sidebar(shared, "evorch-gui:a", private.clone());
    let thread = a.open_thread(shared, "Alpha");
    a.emit(run_started("run-a", &thread));
    a.emit(phase(
        "run-a",
        AgentRunPhase::Pending,
        AgentRunPhase::Running,
    ));
    for event in before {
        a.emit(event.clone());
    }
    let mut b = open_window(shared, "evorch-gui:b");
    for event in after {
        a.emit(event.clone());
    }
    std::fs::copy(&private, shared.sidebar()).expect("publish sidebar");
    b.state.poll_shared_feed();
    b.state.switch_thread(thread.clone()).expect("open");
    (b, thread)
}

fn tool_event(completed: bool) -> Event {
    if completed {
        Event::new(ToolEvent::ToolCompleted {
            tool_name: "shell".into(),
            call_id: "run-a:c1".into(),
            is_error: false,
            output: Some("ok".into()),
            detail: None,
            run_id: Some("run-a".into()),
            parent_call_id: None,
        })
    } else {
        Event::new(ToolEvent::ToolStarted {
            tool_name: "shell".into(),
            call_id: "run-a:c1".into(),
            input: None,
            run_id: Some("run-a".into()),
            parent_call_id: None,
        })
    }
}

#[test]
fn completed_messages_on_both_sides_of_the_restart_each_appear_once() {
    let (b, _) = conversation_across_a_late_sidebar(
        &[reply("run-a", "reply1")],
        &[reply("run-a", "reply2"), reply("run-a", "reply3")],
    );

    assert_eq!(texts(&b.state), ["reply1", "reply2", "reply3"]);
}

#[test]
fn a_streamed_message_split_by_the_restart_is_not_repeated() {
    let delta = |text: &str| {
        Event::new(MessageEvent::MessageDelta {
            run_id: Some("run-a".into()),
            delta: text.into(),
        })
    };
    let (b, _) = conversation_across_a_late_sidebar(
        &[delta("al"), delta("pha ")],
        &[delta("beta "), delta("gamma")],
    );

    assert_eq!(texts(&b.state), ["alpha beta gamma"]);
}

#[test]
fn a_tool_call_split_by_the_restart_is_shown_once_and_completed() {
    let (b, _) = conversation_across_a_late_sidebar(&[tool_event(false)], &[tool_event(true)]);

    let calls: Vec<_> = b
        .state
        .transcript()
        .entries()
        .iter()
        .filter(|entry| matches!(entry, TranscriptEntry::Tool { .. }))
        .collect();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(matches!(
        calls[0],
        TranscriptEntry::Tool { output: Some(output), .. } if output == "ok"
    ));
}
