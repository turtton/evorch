//! A thread row says when another process works on its thread. The evidence is
//! a run another process started or changed that is Running or Pending, in a
//! thread this process does not own; looking at it claims or writes nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use egui_kittest::{Harness, kittest::Queryable};
use event_bus::{AgentRunPhase, Event, EventBus, LifecycleEvent};
use gui::app::WorkbenchState;
use gui::fixture::DemoSource;
use gui::model::composer::ProviderStatus;
use gui::panes::sidebar::sidebar_pane_with_retention;
use runtime::ownership::OwnerHost;
use storage::{Database, Storage, StorageConfig};
use tempfile::TempDir;
use workspace_ui::{ThreadId, ThreadRunPhase, UiSettings};

const EXTERNAL_ROW: &str = "Thread attention: Working in another window or process";
const EXTERNAL_SPINNER: &str = "Thread status: Running in another window or process";
const OWN_SPINNER: &str = "Thread status: Running";

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
}

struct Window {
    state: WorkbenchState<DemoSource>,
    storage: Storage,
    stream: String,
    host: Arc<OwnerHost>,
}

fn open_window(shared: &Shared, stream: &str) -> Window {
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
    let sidebar_path = shared.dir.path().join("sidebar.json");
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

    fn external(&self) -> BTreeSet<ThreadId> {
        self.state.externally_working_threads()
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

fn event_count(shared: &Shared) -> i64 {
    Database::open_read_only(&shared.config.db_path)
        .expect("read-only open")
        .last_event_id()
        .expect("last id")
}

/// Window A owns "Alpha" and runs a conversation in it; window B follows.
fn owner_running_and_follower(shared: &Shared) -> (Window, Window, ThreadId) {
    let mut a = open_window(shared, "evorch-gui:a");
    let thread = a.open_thread(shared, "Alpha");
    a.host
        .start(&thread.to_string())
        .expect("A owns the thread");
    a.emit(run_started("run-a", &thread));
    a.emit(phase(
        "run-a",
        AgentRunPhase::Pending,
        AgentRunPhase::Running,
    ));
    let mut b = open_window(shared, "evorch-gui:b");
    b.state.switch_thread(thread.clone()).expect("B opens it");
    b.state.poll_shared_feed();
    (a, b, thread)
}

/// The sidebar as the workbench draws it, with `unread` marked.
fn render(window: &Window, unread: &[&ThreadId]) -> Harness<'static> {
    let sidebar = window.state.sidebar().clone();
    let phases: BTreeMap<String, ThreadRunPhase> = window.state.thread_phases().clone();
    let external = window.external();
    let unread: BTreeSet<ThreadId> = unread.iter().map(|id| (*id).clone()).collect();
    let none = BTreeSet::new();
    let retention = BTreeMap::new();
    let mut harness = Harness::builder()
        .with_size(egui::vec2(360.0, 360.0))
        .build_ui(move |ui| {
            gui::theme::install(ui.ctx());
            sidebar_pane_with_retention(
                ui, &sidebar, &phases, &none, &none, &unread, &external, &retention,
            );
        });
    // A spinner requests repaints continuously; advance a bounded number of frames.
    harness.run_steps(3);
    harness
}

#[test]
fn a_run_another_process_executes_marks_only_the_thread_it_does_not_own() {
    // Given: A owns a thread and runs in it, B follows the shared log
    let shared = Shared::new();
    let (a, b, thread) = owner_running_and_follower(&shared);

    // Then: B marks the thread as worked on elsewhere, A (the owner) does not
    assert_eq!(b.external(), BTreeSet::from([thread.clone()]));
    assert!(a.external().is_empty());

    // And: B's row says so in words and with the running spinner's name
    let rendered = render(&b, &[]);
    assert!(rendered.query_by_label(EXTERNAL_ROW).is_some());
    assert!(rendered.query_by_label(EXTERNAL_SPINNER).is_some());

    // And: the owner's own row neither claims "another process" nor loses its spinner
    let owner = render(&a, &[]);
    assert!(owner.query_by_label(EXTERNAL_ROW).is_none());
    assert!(owner.query_by_label(EXTERNAL_SPINNER).is_none());
    assert!(owner.query_by_label(OWN_SPINNER).is_some());
}

#[test]
fn the_mark_goes_when_the_run_ends() {
    // Given: B marks a thread A is working on
    let shared = Shared::new();
    let (mut a, mut b, _thread) = owner_running_and_follower(&shared);
    assert_eq!(b.external().len(), 1);

    // When: the run finishes and B follows
    a.emit(phase("run-a", AgentRunPhase::Running, AgentRunPhase::Done));
    b.state.poll_shared_feed();

    // Then: nothing marks the thread any more
    assert!(b.external().is_empty());
    assert!(render(&b, &[]).query_by_label(EXTERNAL_ROW).is_none());
}

#[test]
fn a_run_waiting_for_an_answer_is_not_work_in_progress() {
    // Given: A's run is Running and B marks the thread
    let shared = Shared::new();
    let (mut a, mut b, _thread) = owner_running_and_follower(&shared);

    // When: the run waits for an answer
    a.emit(phase(
        "run-a",
        AgentRunPhase::Running,
        AgentRunPhase::Waiting,
    ));
    b.state.poll_shared_feed();

    // Then: it is left to the question color, not marked as external work
    assert!(b.external().is_empty());

    // And: a run that resumes is marked again
    a.emit(phase(
        "run-a",
        AgentRunPhase::Waiting,
        AgentRunPhase::Running,
    ));
    b.state.poll_shared_feed();
    assert_eq!(b.external().len(), 1);
}

#[test]
fn not_being_in_this_runtime_is_not_evidence() {
    // Given: B has a thread it does not own and a run of its own making in it,
    // and another thread nobody works on
    let shared = Shared::new();
    let mut b = open_window(&shared, "evorch-gui:b");
    let thread = b.open_thread(&shared, "Local");
    let idle = b.state.create_thread("Idle").expect("thread");
    b.emit(run_started("run-b", &thread));
    b.emit(phase(
        "run-b",
        AgentRunPhase::Pending,
        AgentRunPhase::Running,
    ));

    // Then: a Running run no other process produced marks nothing, owned or not
    assert_eq!(
        b.state.thread_phases().get("run-b"),
        Some(&ThreadRunPhase::Running)
    );
    assert!(b.external().is_empty());
    assert!(!b.external().contains(&idle));
}

#[test]
fn runs_restored_as_active_after_a_restart_are_marked_before_any_event_arrives() {
    // Given: A is working
    let shared = Shared::new();
    let (_a, _b, thread) = owner_running_and_follower(&shared);

    // When: a window opens afresh and replays history, without polling the feed
    let c = open_window(&shared, "evorch-gui:c");

    // Then: the restored active run counts as another process's
    assert_eq!(c.external(), BTreeSet::from([thread]));
}

#[test]
fn looking_claims_renews_and_writes_nothing() {
    // Given: A owns the thread; B marks it
    let shared = Shared::new();
    let (a, mut b, thread) = owner_running_and_follower(&shared);
    let id = thread.to_string();
    let owner_before = a.host.attach(&id).expect("owner record");
    let events_before = event_count(&shared);

    // When: B computes and draws the marks repeatedly
    for _ in 0..3 {
        assert_eq!(b.external().len(), 1);
        let _ = render(&b, &[]);
        b.state.poll_shared_feed();
    }

    // Then: the lease, its holder and the event log are as they were
    let owner_after = b.host.attach(&id).expect("owner record");
    assert_eq!(owner_after, owner_before);
    assert_eq!(owner_after.lease.owner_id, a.host.owner_id());
    assert!(b.host.owned_permit(&id).is_err(), "B must not own it");
    assert_eq!(event_count(&shared), events_before);
}

#[test]
fn an_unread_report_outranks_external_work_in_the_row() {
    // Given: B marks a thread as worked on elsewhere
    let shared = Shared::new();
    let (_a, b, thread) = owner_running_and_follower(&shared);

    // When: the same thread also has an unread report
    let rendered = render(&b, &[&thread]);

    // Then: the row names the stronger state only
    assert!(
        rendered
            .query_by_label("Thread attention: Unread report")
            .is_some()
    );
    assert!(rendered.query_by_label(EXTERNAL_ROW).is_none());
}
