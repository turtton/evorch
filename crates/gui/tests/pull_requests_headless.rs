//! ADR 0043 Phase 1 (PR5): the GUI against the real runtime service, storage and
//! `RuntimeCommandSink`, with the pull request source scripted.
//!
//! Each step waits for the event the service publishes after it committed, never for time. The
//! display is then fed the events as a window would receive them and reads the database.

use std::sync::Arc;

use async_trait::async_trait;
use event_bus::{Event, EventBus, EventKind, EventReceiver, PullRequestEvent};
use gui::app::WorkbenchState;
use gui::app::pull_requests::StoragePullRequestReader;
use gui::fixture::DemoSource;
use gui::model::commands::{CommandSink, RecordingSink};
use gui::model::composer::ProviderStatus;
use gui::model::notifications::NotificationKind;
use gui::model::pull_requests::{PrGlyph, PullRequestAction, glyph};
use gui::model::transcript::TranscriptEntry;
use gui::runtime_sink::RuntimeCommandSink;
use providers::{ChatResponse, FinishReason, Message, Role as MessageRole, ToolSpec, Usage};
use runtime::ownership::OwnerHost;
use runtime::pull_requests::{FakePullRequestSource, FetchedPullRequest, PullRequestService};
use runtime::{AgentInvocationContext, AgentModel, AgentRuntime, Role, RunStore, RuntimeError};
use storage::pull_requests::{
    CandidateState, CheckConclusion, CheckKind, CheckStatus, NewPullRequestCandidate,
    PullRequestCheck, PullRequestFacts, PullRequestKey, PullRequestState, ReviewDecision,
};
use workspace_ui::{ProjectId, SidebarState, ThreadId, UiSettings};

#[path = "support/short_private_directory.rs"]
mod short_private_directory;
use short_private_directory::ShortPrivateDirectory;

const THREAD: &str = "t1";
const REPOSITORY: &str = "turtton/evorch";

struct NoModel;

#[async_trait]
impl AgentModel for NoModel {
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        Ok(ChatResponse {
            usage_reported: true,
            message: Message {
                role: MessageRole::Assistant,
                content: Vec::new(),
            },
            finish_reason: FinishReason::Stop,
            usage: Usage::default(),
        })
    }

    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "pull-requests-headless".into()
    }
}

fn key(number: u64) -> PullRequestKey {
    PullRequestKey::github(REPOSITORY, number).unwrap()
}

fn check(name: &str, conclusion: Option<CheckConclusion>) -> PullRequestCheck {
    PullRequestCheck {
        provider_id: name.into(),
        kind: CheckKind::CheckRun,
        name: name.into(),
        status: if conclusion.is_some() {
            CheckStatus::Completed
        } else {
            CheckStatus::InProgress
        },
        conclusion,
        head_sha: sha('a'),
    }
}

fn sha(letter: char) -> String {
    letter.to_string().repeat(40)
}

fn facts(state: PullRequestState, head: char, checks: Vec<PullRequestCheck>) -> PullRequestFacts {
    PullRequestFacts {
        state,
        base_branch: "main".into(),
        head_repository: Some(REPOSITORY.into()),
        head_branch: "feat/gui".into(),
        head_sha: sha(head),
        checks,
        checks_complete: true,
        review_decision: ReviewDecision::None,
    }
}

fn fetched(number: u64, facts: PullRequestFacts) -> FetchedPullRequest {
    FetchedPullRequest {
        requested: key(number),
        canonical: key(number),
        url: format!("https://github.com/{REPOSITORY}/pull/{number}"),
        facts,
        fetched_at_ns: gui::model::pull_requests::now_ns(),
    }
}

fn running(number: u64) -> FetchedPullRequest {
    fetched(
        number,
        facts(PullRequestState::Open, 'a', vec![check("ci", None)]),
    )
}

fn passing(number: u64) -> FetchedPullRequest {
    fetched(
        number,
        facts(
            PullRequestState::Open,
            'a',
            vec![check("ci", Some(CheckConclusion::Success))],
        ),
    )
}

fn failing(number: u64, head: char) -> FetchedPullRequest {
    fetched(
        number,
        facts(
            PullRequestState::Open,
            head,
            vec![check("ci", Some(CheckConclusion::Failure))],
        ),
    )
}

fn merged(number: u64) -> FetchedPullRequest {
    fetched(number, facts(PullRequestState::Merged, 'a', Vec::new()))
}

struct World {
    directory: tempfile::TempDir,
    owners: ShortPrivateDirectory,
    config: storage::StorageConfig,
    storage: storage::Storage,
    bus: Arc<EventBus>,
    runtime: AgentRuntime,
    host: Arc<OwnerHost>,
    source: Arc<FakePullRequestSource>,
    service: PullRequestService,
    events: EventReceiver,
    seen: Vec<Event>,
}

impl World {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let owners = ShortPrivateDirectory::new();
        let config = storage::StorageConfig {
            db_path: directory.path().join("pull-requests.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let bus = Arc::new(EventBus::new(4096));
        let events = bus.subscribe();
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(tools::ToolExecutor::new(bus.clone())),
            Arc::new(NoModel),
        )
        .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
        let host =
            Arc::new(OwnerHost::open(owners.path(), Default::default(), bus.clone()).unwrap());
        let source = Arc::new(FakePullRequestSource::new());
        let service = runtime
            .start_pull_requests(
                host.clone() as Arc<dyn runtime::pull_requests::OwnershipView>,
                source.clone(),
                config::PullRequestsConfig {
                    enabled: true,
                    ..Default::default()
                },
            )
            .unwrap();
        Self {
            directory,
            owners,
            config,
            storage,
            bus,
            runtime,
            host,
            source,
            service,
            events,
            seen: Vec::new(),
        }
    }

    fn sink(&self, host: &Arc<OwnerHost>) -> Box<dyn CommandSink> {
        Box::new(
            RuntimeCommandSink::new(self.runtime.clone(), tokio::runtime::Handle::current())
                .with_ownership(host.clone()),
        )
    }

    /// A window over this world's database whose commands go to `sink`.
    fn window_with(
        &self,
        host: &Arc<OwnerHost>,
        sink: Box<dyn CommandSink>,
    ) -> WorkbenchState<DemoSource> {
        let mut sidebar = SidebarState::default();
        let project = ProjectId::new("project");
        sidebar
            .add_project(project.clone(), "project", self.directory.path())
            .unwrap();
        sidebar.select_project(&project).unwrap();
        sidebar
            .create_thread(ThreadId::new(THREAD), project, "Thread")
            .unwrap();
        sidebar.switch_thread(&ThreadId::new(THREAD)).unwrap();
        let probe_host = host.clone();
        let inline: gui::app::lanes::Spawner = Arc::new(|job| job());
        WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
            .unwrap()
            .with_sidebar(sidebar)
            .with_provider_status(ProviderStatus::Configured)
            .with_ownership(host.clone())
            .with_command_sink(sink)
            .with_pull_requests(
                Arc::new(StoragePullRequestReader::new(self.config.db_path.clone())),
                Some(Arc::new(move |thread: &str| {
                    probe_host.owned_permit(thread).is_ok()
                })),
            )
            .with_pull_request_spawner(inline)
    }

    fn window(&self) -> WorkbenchState<DemoSource> {
        self.window_with(&self.host, self.sink(&self.host))
    }

    /// Receives events until one matches; every event received is kept for the window.
    async fn until(&mut self, wanted: impl Fn(&PullRequestEvent) -> bool) {
        loop {
            let event = self.events.recv().await.unwrap();
            let done = matches!(&event.kind, EventKind::PullRequest(change) if wanted(change));
            self.seen.push(event);
            if done {
                return;
            }
        }
    }

    /// Hands the received events to the window and lets it read the database.
    fn show(&mut self, state: &mut WorkbenchState<DemoSource>) {
        state.apply_events(std::mem::take(&mut self.seen));
        settle(state);
    }

    fn database(&self) -> storage::Database {
        storage::Database::open_read_only(&self.config.db_path).unwrap()
    }
}

fn settle(state: &mut WorkbenchState<DemoSource>) {
    for _ in 0..4 {
        state.pump_pull_requests();
    }
}

fn changed(thread: &'static str) -> impl Fn(&PullRequestEvent) -> bool {
    move |event| matches!(event, PullRequestEvent::Changed { thread_id, .. } if thread_id == thread)
}

fn link_action(locator: &str) -> PullRequestAction {
    PullRequestAction::Link {
        thread_id: THREAD.into(),
        locator: locator.into(),
    }
}

fn notices(state: &WorkbenchState<DemoSource>) -> Vec<String> {
    state
        .transcript()
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::Notice { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

// Given: a window with the service running / When: "Link PR…" links a pull request, Refresh
// finds it done, and a fork is made before Unlink / Then: the bar and the badge follow the
// stored facts, and Unlink removes them from the thread while the fork keeps its link
#[tokio::test]
async fn link_refresh_unlink_and_the_fork_keeps_its_link() {
    let mut world = World::new();
    let mut window = world.window();
    world.source.push_ok(&key(12), running(12));
    world.source.push_ok(&key(12), passing(12));
    world.source.repeat_last();
    settle(&mut window);
    assert!(window.pull_request_model().linked(THREAD).is_none());

    // When: the dialog is opened and its Link is pressed.
    window.apply_pull_request_action(PullRequestAction::OpenLinkDialog {
        thread_id: THREAD.into(),
    });
    settle(&mut window);
    assert!(window.pull_request_dialog().is_some());
    window.apply_pull_request_action(link_action("https://github.com/turtton/evorch/pull/12"));
    assert!(
        window.pull_request_dialog().is_none(),
        "the dialog closes on Link"
    );
    world.until(changed(THREAD)).await;
    world.show(&mut window);

    // Then: the bar shows the running pull request and the thread list a badge.
    let view = window.pull_request_model().linked(THREAD).expect("linked");
    assert_eq!(view.number(), Some(12));
    assert_eq!(glyph(view), Some(PrGlyph::Pending));
    assert!(view.tracked_here, "the window that linked it polls it");

    // When: Refresh fetches again.
    window.apply_pull_request_action(PullRequestAction::Refresh {
        thread_id: THREAD.into(),
    });
    world.until(changed(THREAD)).await;
    world.show(&mut window);
    assert_eq!(
        glyph(window.pull_request_model().linked(THREAD).unwrap()),
        Some(PrGlyph::Passing)
    );

    // And: a fork is made. The UI thread writes nothing (this runtime runs one thread, so the
    // queued pin cannot have run yet), and the person's Unlink right after it still finds the
    // parent's link to pin: the operations run in the order they were issued.
    let before = world
        .database()
        .pull_request_thread_state(THREAD)
        .unwrap()
        .active_link
        .unwrap();
    let child = window.fork_thread(ThreadId::new(THREAD)).unwrap();
    assert!(
        world
            .storage
            .handle()
            .pull_request_inheritance(&child.to_string())
            .unwrap()
            .is_none(),
        "forking wrote to the database on the calling thread"
    );
    window.apply_pull_request_action(PullRequestAction::Unlink {
        thread_id: THREAD.into(),
    });
    world
        .until(|event| matches!(event, PullRequestEvent::Unlinked { thread_id, .. } if thread_id == THREAD))
        .await;
    let pinned = world
        .storage
        .handle()
        .pull_request_inheritance(&child.to_string())
        .unwrap()
        .expect("the fork's inheritance was pinned before the Unlink ran");
    assert_eq!(
        pinned.state,
        storage::pull_requests::InheritanceState::Pending
    );
    assert_eq!(pinned.parent_link_id, before.link_id);

    // When: the fork's owner starts, the service gives it the link the parent had.
    world.host.start(&child.to_string()).unwrap();
    world.service.notify_changed();
    let child_id = child.to_string();
    world
        .until(
            move |event| matches!(event, PullRequestEvent::Linked { thread_id, .. } if *thread_id == child_id),
        )
        .await;
    world.show(&mut window);

    // Then: the parent has neither bar nor badge; the fork's link remains.
    assert!(window.pull_request_model().linked(THREAD).is_none());
    assert!(
        window
            .pull_request_model()
            .linked(&child.to_string())
            .is_some()
    );
    let state = world.database().pull_request_thread_state(THREAD).unwrap();
    assert!(state.active_link.is_none());
    assert!(
        world
            .database()
            .pull_request_thread_state(&child.to_string())
            .unwrap()
            .active_link
            .is_some()
    );
}

// Given: a window whose view of the thread is old / When: the link was replaced meanwhile and the
// window presses Unlink / Then: the revision refuses it, the link stays, and the person is told
#[tokio::test]
async fn an_unlink_from_an_old_view_is_refused_by_the_revision() {
    let mut world = World::new();
    let mut window = world.window();
    world.source.push_ok(&key(12), running(12));
    world.source.push_ok(&key(13), running(13));
    world.source.repeat_last();
    settle(&mut window);
    window.apply_pull_request_action(link_action("turtton/evorch#12"));
    world.until(changed(THREAD)).await;
    world.show(&mut window);
    let stale = window
        .pull_request_model()
        .linked(THREAD)
        .unwrap()
        .lifecycle_revision;

    // When: another window relinks (the service is called as that window's command would),
    // and this window, still showing the old link, presses Unlink.
    let permit = world.host.owned_permit(THREAD).unwrap();
    world
        .service
        .link(THREAD, "turtton/evorch#13", stale, &permit)
        .await
        .unwrap();
    world.seen.clear();
    window.apply_pull_request_action(PullRequestAction::Unlink {
        thread_id: THREAD.into(),
    });
    while !notices(&window)
        .iter()
        .any(|text| text.contains("changed meanwhile"))
    {
        tokio::task::yield_now().await;
        window.poll_command_sink();
    }

    // Then: the newer link is untouched.
    let state = world.database().pull_request_thread_state(THREAD).unwrap();
    assert_eq!(state.active_link.unwrap().key, key(13));
}

// Given: a candidate a tool output suggested / When: Link PR is pressed on its chip / Then: the
// thread is linked to the parsed pull request and the chip reads as linked
#[tokio::test]
async fn a_candidate_chip_links_with_one_click_and_becomes_a_linked_line() {
    let mut world = World::new();
    let mut window = world.window();
    world.source.push_ok(&key(21), running(21));
    world.source.repeat_last();
    let presented = world
        .storage
        .handle()
        .present_pull_request_candidate(NewPullRequestCandidate {
            thread_id: THREAD.into(),
            key: key(21),
            source_run_id: "run-1".into(),
            source_call_id: "call-1".into(),
            at_ns: 1,
        })
        .unwrap();
    window.apply_events([Event::new(PullRequestEvent::CandidateChanged {
        thread_id: THREAD.into(),
        link_revision: 0,
        transition_seq: 0,
        pull_request: event_bus::PullRequestRef {
            provider: "github".into(),
            host: "github.com".into(),
            repository: REPOSITORY.into(),
            number: 21,
        },
        candidate_id: presented.candidate.candidate_id,
        state: event_bus::PullRequestCandidateState::Presented,
    })]);
    settle(&mut window);
    let chips =
        gui::model::pull_requests::chips_by_call(window.pull_request_model().view(THREAD).unwrap());
    let chip = &chips[&("run-1".to_owned(), "call-1".to_owned())][0];
    assert_eq!(chip.label, "turtton/evorch#21");
    assert_eq!(chip.state, gui::model::pull_requests::ChipState::Offered);

    // When: the chip's Link PR is pressed.
    window.apply_pull_request_action(PullRequestAction::Confirm {
        thread_id: THREAD.into(),
        candidate_id: presented.candidate.candidate_id,
    });
    world.until(changed(THREAD)).await;
    world.show(&mut window);

    // Then: the thread is linked, and the chip is one linked line.
    assert!(window.pull_request_model().linked(THREAD).is_some());
    let chips =
        gui::model::pull_requests::chips_by_call(window.pull_request_model().view(THREAD).unwrap());
    assert_eq!(
        chips[&("run-1".to_owned(), "call-1".to_owned())][0].state,
        gui::model::pull_requests::ChipState::Linked
    );
    assert_eq!(
        world
            .database()
            .pull_request_candidates(THREAD, Some(CandidateState::Confirmed))
            .unwrap()
            .len(),
        1
    );
}

// Given: a CI failure after a pass / Then: the bar expands once; closed by the person it stays
// closed while the failure is polled again
#[tokio::test]
async fn a_new_failure_expands_the_bar_once() {
    let mut world = World::new();
    let mut window = world.window();
    world.source.push_ok(&key(12), passing(12));
    world.source.push_ok(&key(12), failing(12, 'b'));
    world.source.push_ok(&key(12), failing(12, 'c'));
    world.source.repeat_last();
    settle(&mut window);
    window.apply_pull_request_action(link_action("turtton/evorch#12"));
    world.until(changed(THREAD)).await;
    world.show(&mut window);
    assert!(!window.pull_request_model().bar(THREAD).unwrap().expanded);

    // When: the checks fail.
    window.apply_pull_request_action(PullRequestAction::Refresh {
        thread_id: THREAD.into(),
    });
    world.until(changed(THREAD)).await;
    world.show(&mut window);
    assert!(window.pull_request_model().bar(THREAD).unwrap().expanded);

    // And: the person closes it and the failure is polled again with a new commit.
    window.apply_pull_request_action(PullRequestAction::ToggleExpanded {
        thread_id: THREAD.into(),
    });
    assert!(!window.pull_request_model().bar(THREAD).unwrap().expanded);
    window.apply_pull_request_action(PullRequestAction::Refresh {
        thread_id: THREAD.into(),
    });
    world.until(changed(THREAD)).await;
    world.show(&mut window);

    // Then: it stays closed.
    let view = window.pull_request_model().linked(THREAD).unwrap();
    assert_eq!(glyph(view), Some(PrGlyph::Failing));
    assert!(!window.pull_request_model().bar(THREAD).unwrap().expanded);
}

fn change(thread: &str, revision: u64, seq: u64, kinds: &[&str]) -> Event {
    Event::new(PullRequestEvent::Changed {
        thread_id: thread.into(),
        link_revision: revision,
        transition_seq: seq,
        pull_request: event_bus::PullRequestRef {
            provider: "github".into(),
            host: "github.com".into(),
            repository: REPOSITORY.into(),
            number: 12,
        },
        fields: vec![event_bus::PullRequestChangedField::Checks],
        notifications: kinds.iter().map(|kind| (*kind).to_owned()).collect(),
    })
}

fn kinds(state: &WorkbenchState<DemoSource>) -> Vec<NotificationKind> {
    state
        .notifications()
        .items()
        .map(|item| item.kind.clone())
        .collect()
}

/// A thread this process holds, linked to #12 in the database; returns its link revision.
fn linked_and_owned(world: &World) -> u64 {
    world.host.start(THREAD).unwrap();
    world
        .storage
        .handle()
        .link_pull_request(storage::pull_requests::LinkPullRequest {
            thread_id: THREAD.into(),
            key: key(12),
            expected_revision: 0,
            at_ns: 1,
        })
        .unwrap()
        .link
        .link_revision
}

fn unlinked_event(revision: u64) -> Event {
    Event::new(PullRequestEvent::Unlinked {
        thread_id: THREAD.into(),
        link_revision: revision,
        transition_seq: 3,
        pull_request: event_bus::PullRequestRef {
            provider: "github".into(),
            host: "github.com".into(),
            repository: REPOSITORY.into(),
            number: 12,
        },
        reason: event_bus::PullRequestLinkEnd::Unlinked,
    })
}

// Given: events the runtime claimed notifications for / Then: once the database confirms the link
// and the owner, one notification per thread and transition
#[tokio::test]
async fn a_transition_notifies_once_after_the_database_confirms_it() {
    let world = World::new();
    let revision = linked_and_owned(&world);
    let mut window = world.window_with(&world.host, Box::new(RecordingSink::default()));
    let thread = ThreadId::new(THREAD);

    // When: the same transition arrives twice, then another kind.
    window.apply_events([change(THREAD, revision, 1, &["checks_failed"])]);
    window.apply_events([change(THREAD, revision, 1, &["checks_failed"])]);
    window.apply_events([change(THREAD, revision, 2, &["merged"])]);
    assert_eq!(
        window.notifications().items().count(),
        0,
        "nothing is raised before the database has been read again"
    );
    settle(&mut window);

    // Then: each is shown once, in the existing rows.
    assert_eq!(
        kinds(&window),
        [
            NotificationKind::PullRequestChecksFailed {
                thread_id: thread.clone()
            },
            NotificationKind::PullRequestMerged {
                thread_id: thread.clone()
            },
        ]
    );
    assert!(
        window
            .notifications()
            .items()
            .next()
            .unwrap()
            .summary
            .starts_with("CI failed on turtton/evorch#12")
    );
}

// Given: an event whose link another window unlinked before this window read the database /
// Then: nothing is raised, whether the event was folded before or after the Unlink; a later
// link's event is
#[tokio::test]
async fn a_late_event_for_a_link_another_window_ended_never_notifies() {
    let world = World::new();
    let revision = linked_and_owned(&world);
    let mut window = world.window_with(&world.host, Box::new(RecordingSink::default()));
    settle(&mut window);

    // When: the event is folded, then the link is ended elsewhere before the read.
    window.apply_events([change(THREAD, revision, 1, &["checks_failed"])]);
    let ended = world
        .storage
        .handle()
        .unlink_pull_request(THREAD, revision, 2)
        .unwrap();
    // And: an event of the same ended link arrives later still.
    window.apply_events([change(THREAD, revision, 4, &["merged"])]);
    settle(&mut window);

    // Then: nothing is raised.
    assert_eq!(window.notifications().items().count(), 0);

    // And: a later link of the thread notifies for its own events.
    let relinked = world
        .storage
        .handle()
        .link_pull_request(storage::pull_requests::LinkPullRequest {
            thread_id: THREAD.into(),
            key: key(12),
            expected_revision: ended.lifecycle_revision,
            at_ns: 3,
        })
        .unwrap();
    window.apply_events([change(
        THREAD,
        relinked.link.link_revision,
        1,
        &["checks_failed"],
    )]);
    settle(&mut window);
    assert_eq!(window.notifications().items().count(), 1);
}

// Given: a Changed event waiting to be confirmed / When: the shared feed delivers the Unlinked
// of that link from another window / Then: it is recorded as ended and nothing is raised, even
// though the database read still shows the link
#[tokio::test]
async fn a_shared_unlink_ends_the_link_for_the_notifications_waiting_on_it() {
    let world = World::new();
    let revision = linked_and_owned(&world);
    let mut window = world
        .window_with(&world.host, Box::new(RecordingSink::default()))
        .with_manual_shared_feed(world.config.db_path.clone(), "viewer".into())
        .unwrap();
    settle(&mut window);

    window.apply_events([change(THREAD, revision, 1, &["checks_failed"])]);
    world
        .storage
        .handle()
        .append_stream_event("elsewhere", &unlinked_event(revision), None)
        .unwrap();
    window.poll_shared_feed();
    settle(&mut window);

    assert_eq!(window.notifications().items().count(), 0);
}

// Given: a thread this process does not own / When: an event with claimed notifications reaches
// it / Then: the owner check refuses it
#[tokio::test]
async fn a_notification_needs_this_process_to_hold_the_thread() {
    let world = World::new();
    let revision = linked_and_owned(&world);
    let second = Arc::new(
        OwnerHost::open(world.owners.path(), Default::default(), world.bus.clone()).unwrap(),
    );
    let mut window = world.window_with(&second, Box::new(RecordingSink::default()));

    window.apply_events([change(THREAD, revision, 1, &["checks_failed"])]);
    settle(&mut window);

    assert_eq!(window.notifications().items().count(), 0);
}

// Given: a candidate / When: Ignore is pressed in a window that does not hold the thread, then in
// the one that does / Then: the first is refused as chat is and changes nothing; the second
// dismisses it
#[tokio::test]
async fn ignoring_a_candidate_needs_the_threads_write_mode() {
    let mut world = World::new();
    world.host.start(THREAD).unwrap();
    let presented = world
        .storage
        .handle()
        .present_pull_request_candidate(NewPullRequestCandidate {
            thread_id: THREAD.into(),
            key: key(21),
            source_run_id: "run-1".into(),
            source_call_id: "call-1".into(),
            at_ns: 1,
        })
        .unwrap();
    let id = presented.candidate.candidate_id;
    let dismiss = || PullRequestAction::Dismiss {
        thread_id: THREAD.into(),
        candidate_id: id,
    };
    let second = Arc::new(
        OwnerHost::open(world.owners.path(), Default::default(), world.bus.clone()).unwrap(),
    );
    let mut other = world.window_with(&second, world.sink(&second));
    settle(&mut other);

    other.apply_pull_request_action(dismiss());
    other.poll_command_sink();

    assert!(
        notices(&other)
            .iter()
            .any(|text| text == "Another window holds this thread in write mode"),
        "{:?}",
        notices(&other)
    );
    let state = |world: &World| {
        world
            .database()
            .pull_request_candidates(THREAD, None)
            .unwrap()[0]
            .state
    };
    assert_eq!(state(&world), CandidateState::Presented);

    let mut owner = world.window();
    settle(&mut owner);
    owner.apply_pull_request_action(dismiss());
    world
        .until(|event| matches!(event, PullRequestEvent::CandidateChanged { .. }))
        .await;
    assert_eq!(state(&world), CandidateState::Dismissed);
}

// Given: a thread another window owns / When: its events arrive through the shared feed / Then:
// this window shows the stored snapshot and fetches, republishes and notifies nothing
#[tokio::test]
async fn a_window_that_does_not_own_the_thread_only_displays() {
    let mut world = World::new();
    let mut owner = world.window();
    world.source.push_ok(&key(12), failing(12, 'b'));
    world.source.repeat_last();
    settle(&mut owner);
    owner.apply_pull_request_action(link_action("turtton/evorch#12"));
    world.until(changed(THREAD)).await;
    let calls_by_the_owner = world.source.calls().len();

    // And: a second window of another process, with no service of its own.
    let other_owners = ShortPrivateDirectory::new();
    let other_bus = Arc::new(EventBus::new(64));
    let other_host =
        Arc::new(OwnerHost::open(other_owners.path(), Default::default(), other_bus).unwrap());
    let mut viewer = world
        .window_with(&other_host, Box::new(RecordingSink::default()))
        .with_manual_shared_feed(world.config.db_path.clone(), "viewer".into())
        .unwrap();

    // When: the owner's events were persisted and the viewer follows the feed.
    let changed_event = change(THREAD, 1, 1, &["checks_failed", "merged"]);
    world
        .storage
        .handle()
        .append_stream_event("owner", &changed_event, None)
        .unwrap();
    viewer.poll_shared_feed();
    settle(&mut viewer);

    // Then: the stored facts are shown, as untracked here.
    let view = viewer.pull_request_model().linked(THREAD).expect("shown");
    assert!(!view.tracked_here);
    assert_eq!(glyph(view), Some(PrGlyph::Failing));
    // And: nothing was fetched, commanded or notified by the viewer, and the bus stayed quiet.
    assert!(viewer.issued().is_empty());
    assert_eq!(viewer.notifications().items().count(), 0);
    assert_eq!(world.source.calls().len(), calls_by_the_owner);
    world.seen.clear();
    world.bus.emit(Event::new(event_bus::DiagnosticEvent {
        source: "pull-requests-headless".into(),
        severity: event_bus::DiagnosticSeverity::Info,
        code: "sentinel".into(),
        detail: String::new(),
        run_id: None,
        thread_id: None,
        call_id: None,
    }));
    let mut quiet = true;
    loop {
        let event = world.events.recv().await.unwrap();
        if matches!(event.kind, EventKind::Diagnostic(_)) {
            break;
        }
        quiet &= !matches!(event.kind, EventKind::PullRequest(_));
    }
    assert!(
        quiet,
        "no pull request event was published by following the feed"
    );
}

// Given: a link stored before a restart and no owner now / When: a new window opens / Then: the
// bar is restored as not tracked; Refresh takes the thread, and polling resumes
#[tokio::test]
async fn after_a_restart_the_bar_is_restored_and_refresh_takes_the_thread() {
    let mut world = World::new();
    // The link was made by an earlier process; nobody holds the thread now.
    world
        .storage
        .handle()
        .link_pull_request(storage::pull_requests::LinkPullRequest {
            thread_id: THREAD.into(),
            key: key(12),
            expected_revision: 0,
            at_ns: 1,
        })
        .unwrap();
    let begun = world
        .storage
        .handle()
        .begin_pull_request_fetch(&key(12))
        .unwrap();
    world
        .storage
        .handle()
        .commit_pull_request_fetch(&key(12), begun, passing(12).facts, 1)
        .unwrap();
    let mut window = world.window();
    settle(&mut window);

    // Then: the bar is back, and says this window does not track it.
    let view = window
        .pull_request_model()
        .linked(THREAD)
        .expect("restored");
    assert!(!view.tracked_here);
    assert_eq!(glyph(view), Some(PrGlyph::Passing));
    assert!(world.host.owned_permit(THREAD).is_err());

    // When: Refresh is pressed.
    world.source.push_ok(&key(12), failing(12, 'b'));
    world.source.repeat_last();
    window.apply_pull_request_action(PullRequestAction::Refresh {
        thread_id: THREAD.into(),
    });
    world.until(changed(THREAD)).await;
    world.show(&mut window);

    // Then: this process holds the thread and the fetch happened.
    assert!(world.host.owned_permit(THREAD).is_ok());
    let view = window.pull_request_model().linked(THREAD).unwrap();
    assert!(view.tracked_here);
    assert_eq!(glyph(view), Some(PrGlyph::Failing));
    assert!(!world.source.calls().is_empty());
}

// Given: another window holds the thread in write mode / When: Refresh is pressed here / Then:
// the chat error is shown and nothing is fetched
#[tokio::test]
async fn refresh_is_refused_while_another_window_holds_the_thread() {
    let world = World::new();
    world
        .storage
        .handle()
        .link_pull_request(storage::pull_requests::LinkPullRequest {
            thread_id: THREAD.into(),
            key: key(12),
            expected_revision: 0,
            at_ns: 1,
        })
        .unwrap();
    // The first host holds the thread; the window below belongs to a second host.
    world.host.start(THREAD).unwrap();
    let second = Arc::new(
        OwnerHost::open(world.owners.path(), Default::default(), world.bus.clone()).unwrap(),
    );
    let mut window = world.window_with(&second, world.sink(&second));
    settle(&mut window);

    window.apply_pull_request_action(PullRequestAction::Refresh {
        thread_id: THREAD.into(),
    });

    assert!(
        notices(&window)
            .iter()
            .any(|text| text == "Another window holds this thread in write mode"),
        "{:?}",
        notices(&window)
    );
    assert!(world.source.calls().is_empty());
}

// Given: a window with no link / Then: nothing about pull requests is shown
#[tokio::test]
async fn a_thread_without_a_link_has_no_bar_and_no_badge() {
    let world = World::new();
    let mut window = world.window();
    settle(&mut window);
    assert!(window.pull_request_model().linked(THREAD).is_none());
    assert_eq!(window.pull_request_model().linked_views().count(), 0);
}

// Given: the owner window tracking a pull request that then gets merged / Then: the runtime's
// claimed "merged" becomes one notification with the purple glyph class, and the same events
// delivered again add nothing
#[tokio::test]
async fn a_merge_the_runtime_found_notifies_once() {
    let mut world = World::new();
    let mut window = world.window();
    world.source.push_ok(&key(12), running(12));
    world.source.push_ok(&key(12), merged(12));
    world.source.repeat_last();
    settle(&mut window);
    window.apply_pull_request_action(link_action("turtton/evorch#12"));
    world.until(changed(THREAD)).await;
    world.show(&mut window);
    assert_eq!(
        window.notifications().items().count(),
        0,
        "the first fetch is a baseline"
    );

    window.apply_pull_request_action(PullRequestAction::Refresh {
        thread_id: THREAD.into(),
    });
    world.until(changed(THREAD)).await;
    let events = world.seen.clone();
    world.show(&mut window);

    assert_eq!(
        glyph(window.pull_request_model().linked(THREAD).unwrap()),
        Some(PrGlyph::Merged)
    );
    assert_eq!(
        kinds(&window),
        [NotificationKind::PullRequestMerged {
            thread_id: ThreadId::new(THREAD)
        }]
    );
    window.apply_events(events);
    assert_eq!(window.notifications().items().count(), 1);
}
