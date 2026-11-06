//! Tests of the pull request service.
//!
//! Time is virtual (`start_paused`): a test moves it with `advance` and lets the service's tasks
//! run with `settle`; it waits for what the service publishes, never for a duration. The
//! blocking storage and registry calls run inline under `cfg(test)`, so the clock cannot move
//! while one is outstanding. "Nothing happened" is shown by a marker event published after
//! the quiet point: everything the service emitted before it is already in the receiver.

mod candidates;
mod inheritance;
mod ownership;
mod review;
mod tracking;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use config::PullRequestsConfig;
use event_bus::{Event, EventBus, EventKind, EventReceiver, PullRequestEvent};
use storage::pull_requests::{
    CheckConclusion, CheckKind, CheckStatus, LinkPullRequest, PullRequestCheck, PullRequestFacts,
    PullRequestKey, PullRequestState, ReviewDecision,
};
use storage::{Storage, StorageConfig, StorageHandle};
use tempfile::TempDir;

use super::*;
use crate::ownership::OwnerHost;
use crate::pull_requests::{FakePullRequestSource, FetchedPullRequest};

pub(super) const REPOSITORY: &str = "turtton/evorch";
const MARKER_THREAD: &str = "__marker__";

pub(super) fn key(number: u64) -> PullRequestKey {
    PullRequestKey::github(REPOSITORY, number).unwrap()
}

pub(super) fn check(
    name: &str,
    status: CheckStatus,
    conclusion: Option<CheckConclusion>,
) -> PullRequestCheck {
    PullRequestCheck {
        provider_id: name.into(),
        kind: CheckKind::CheckRun,
        name: name.into(),
        status,
        conclusion,
        head_sha: "a".repeat(40),
    }
}

pub(super) fn running() -> Vec<PullRequestCheck> {
    vec![check("ci", CheckStatus::InProgress, None)]
}

pub(super) fn passing() -> Vec<PullRequestCheck> {
    vec![check(
        "ci",
        CheckStatus::Completed,
        Some(CheckConclusion::Success),
    )]
}

pub(super) fn failing() -> Vec<PullRequestCheck> {
    vec![check(
        "ci",
        CheckStatus::Completed,
        Some(CheckConclusion::Failure),
    )]
}

pub(super) fn facts(state: PullRequestState, checks: Vec<PullRequestCheck>) -> PullRequestFacts {
    PullRequestFacts {
        state,
        base_branch: "main".into(),
        head_repository: Some(REPOSITORY.into()),
        head_branch: "feat".into(),
        head_sha: "a".repeat(40),
        checks,
        checks_complete: true,
        review_decision: ReviewDecision::None,
    }
}

pub(super) fn fetched(number: u64, facts: PullRequestFacts) -> FetchedPullRequest {
    FetchedPullRequest {
        requested: key(number),
        canonical: key(number),
        url: format!("https://github.com/{REPOSITORY}/pull/{number}"),
        display: Default::default(),
        facts,
        fetched_at_ns: 1_000,
    }
}

/// `fetched` with a title for display.
pub(super) fn titled(mut fetched: FetchedPullRequest, title: &str) -> FetchedPullRequest {
    fetched.display = storage::pull_requests::PullRequestDisplay {
        title: storage::pull_requests::DisplayTitle::from_untrusted(title),
    };
    fetched
}

pub(super) fn open_running(number: u64) -> FetchedPullRequest {
    fetched(number, facts(PullRequestState::Open, running()))
}

pub(super) fn open_passing(number: u64) -> FetchedPullRequest {
    fetched(number, facts(PullRequestState::Open, passing()))
}

pub(super) fn enabled() -> PullRequestsConfig {
    PullRequestsConfig {
        enabled: true,
        ..PullRequestsConfig::default()
    }
}

/// A clock the test sets.
pub(super) struct ManualClock(AtomicI64);

impl ManualClock {
    pub(super) fn advance(&self, seconds: i64) {
        self.0.fetch_add(seconds * 1_000_000_000, Ordering::SeqCst);
    }
}

impl WallClock for ManualClock {
    fn now_ns(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// What the service reads about threads.
#[derive(Default)]
pub(super) struct FakeEnvironment {
    runs: Mutex<HashMap<String, String>>,
    directories: Mutex<Vec<PathBuf>>,
    directory_requests: std::sync::atomic::AtomicUsize,
    resolutions: std::sync::atomic::AtomicUsize,
    on_directories: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    resolution: Mutex<Option<crate::pull_requests::CheckoutResolution>>,
}

impl FakeEnvironment {
    pub(super) fn set_resolution(&self, resolution: crate::pull_requests::CheckoutResolution) {
        *self.resolution.lock().unwrap() = Some(resolution);
    }
    pub(super) fn set_directories(&self, directories: Vec<PathBuf>) {
        *self.directories.lock().unwrap() = directories;
    }

    /// Runs `action` the first time the service asks for the directories, as if the world changed
    /// while the answer was being looked up.
    pub(super) fn on_directories(&self, action: impl FnOnce() + Send + 'static) {
        *self.on_directories.lock().unwrap() = Some(Box::new(action));
    }

    /// How many times the service went on to run git.
    pub(super) fn resolutions(&self) -> usize {
        self.resolutions.load(Ordering::SeqCst)
    }

    /// How many times the service asked where a thread's checkouts are (it asks only when it
    /// is about to run git).
    pub(super) fn directory_requests(&self) -> usize {
        self.directory_requests.load(Ordering::SeqCst)
    }

    pub(super) fn bind(&self, run: &str, thread: &str) {
        self.runs
            .lock()
            .unwrap()
            .insert(run.to_owned(), thread.to_owned());
    }
}

impl ThreadEnvironment for FakeEnvironment {
    fn thread_of_run(&self, run_id: &str) -> Option<String> {
        self.runs.lock().unwrap().get(run_id).cloned()
    }

    fn checkout_directories(&self, _thread_id: &str) -> Vec<PathBuf> {
        self.directory_requests.fetch_add(1, Ordering::SeqCst);
        let action = self.on_directories.lock().unwrap().take();
        if let Some(action) = action {
            action();
        }
        self.directories.lock().unwrap().clone()
    }

    fn resolve_checkout(
        &self,
        directories: &[PathBuf],
        head_repository: Option<&str>,
        head_branch: &str,
        budget: Duration,
    ) -> crate::pull_requests::CheckoutResolution {
        self.resolutions.fetch_add(1, Ordering::SeqCst);
        if let Some(resolution) = self.resolution.lock().unwrap().clone() {
            return resolution;
        }
        crate::pull_requests::checkout::resolve_blocking(
            directories,
            head_repository,
            head_branch,
            budget,
        )
    }
}

type PermitAction = Box<dyn FnOnce(&OwnerPermit) + Send>;

/// Actions the ownership view runs while the scheduler plans, as if the world changed while the
/// registry was being read.
#[derive(Default)]
pub(super) struct Hooks {
    before_permit: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    after_permit: Mutex<Option<PermitAction>>,
    during_confirm: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl Hooks {
    /// Runs `action` just before the next permit is looked up.
    pub(super) fn before_permit(&self, action: impl FnOnce() + Send + 'static) {
        *self.before_permit.lock().unwrap() = Some(Box::new(action));
    }

    /// Runs `action` in the middle of the next check that a permit still holds its thread, as if
    /// the registry were busy and the world changed while the check waited.
    pub(super) fn during_confirm(&self, action: impl FnOnce() + Send + 'static) {
        *self.during_confirm.lock().unwrap() = Some(Box::new(action));
    }

    /// Runs `action` with the next permit that was found, before it is handed to the plan.
    pub(super) fn after_permit(&self, action: impl FnOnce(&OwnerPermit) + Send + 'static) {
        *self.after_permit.lock().unwrap() = Some(Box::new(action));
    }
}

struct HookedOwnership {
    host: Arc<OwnerHost>,
    hooks: Arc<Hooks>,
}

impl OwnershipView for HookedOwnership {
    fn owned_permit(&self, thread_id: &str) -> Option<OwnerPermit> {
        let before = self.hooks.before_permit.lock().unwrap().take();
        if let Some(action) = before {
            action();
        }
        let permit = OwnershipView::owned_permit(&*self.host, thread_id)?;
        let after = self.hooks.after_permit.lock().unwrap().take();
        if let Some(action) = after {
            action(&permit);
        }
        Some(permit)
    }

    fn confirms(&self, permit: &OwnerPermit) -> bool {
        let during = self.hooks.during_confirm.lock().unwrap().take();
        if let Some(action) = during {
            action();
        }
        crate::pull_requests::ownership::permit_holds(permit)
    }
}

/// A database and an ownership registry that several "processes" share.
pub(super) struct World {
    pub(super) dir: TempDir,
    pub(super) clock: Arc<ManualClock>,
}

/// One "process": its own storage connection, host, bus, source and service.
pub(super) struct Process {
    pub(super) _storage: Storage,
    pub(super) storage_handle: StorageHandle,
    pub(super) host: Arc<OwnerHost>,
    pub(super) bus: Arc<EventBus>,
    pub(super) source: FakePullRequestSource,
    pub(super) environment: Arc<FakeEnvironment>,
    pub(super) hooks: Arc<Hooks>,
    pub(super) service: PullRequestService,
    pub(super) events: EventReceiver,
}

impl World {
    pub(super) fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
            clock: Arc::new(ManualClock(AtomicI64::new(10_000_000_000))),
        }
    }

    pub(super) fn process(&self, settings: PullRequestsConfig) -> Process {
        self.process_with_logs(
            settings,
            Arc::new(crate::pull_requests::FakePullRequestLogSource::new()),
        )
    }

    pub(super) fn process_with_logs(
        &self,
        settings: PullRequestsConfig,
        logs: Arc<dyn crate::pull_requests::PullRequestLogSource>,
    ) -> Process {
        let storage = Storage::open(StorageConfig {
            db_path: self.dir.path().join("evorch.db"),
            ..StorageConfig::default()
        })
        .unwrap();
        let bus = Arc::new(EventBus::new(4096));
        let host = Arc::new(
            OwnerHost::open(
                &self.dir.path().join("owners"),
                config::OwnershipConfig::default(),
                Arc::clone(&bus),
            )
            .unwrap(),
        );
        let source = FakePullRequestSource::new();
        let environment = Arc::new(FakeEnvironment::default());
        let hooks = Arc::new(Hooks::default());
        let events = bus.subscribe();
        let service = PullRequestService::start_with_log_source(
            ServiceParts {
                storage: storage.handle(),
                source: Arc::new(source.clone()),
                ownership: Arc::new(HookedOwnership {
                    host: Arc::clone(&host),
                    hooks: Arc::clone(&hooks),
                }),
                environment: Arc::clone(&environment) as Arc<dyn ThreadEnvironment>,
                bus: Arc::clone(&bus),
                settings,
                clock: Arc::clone(&self.clock) as Arc<dyn WallClock>,
            },
            logs,
        )
        .unwrap();
        Process {
            storage_handle: storage.handle(),
            _storage: storage,
            host,
            bus,
            source,
            environment,
            hooks,
            service,
            events,
        }
    }
}

impl Process {
    /// Runs `action` when the service next waits on the blocking pool at `stage`, before the
    /// work runs: the world changes while the work is queued.
    pub(super) fn at_stage(&self, stage: Stage, action: impl FnOnce() + Send + 'static) {
        self.service
            .inner()
            .stage_hooks
            .lock()
            .unwrap()
            .insert(stage, Box::new(action));
    }

    /// The permit for `thread`, starting its lease the way chat does when nobody owns it.
    pub(super) fn permit(&self, thread: &str) -> OwnerPermit {
        self.host
            .owned_permit(thread)
            .or_else(|_| self.host.start(thread))
            .unwrap()
    }

    pub(super) fn state(&self, thread: &str) -> storage::pull_requests::ThreadPullRequestState {
        self.storage_handle
            .pull_request_thread_state(thread)
            .unwrap()
    }

    /// Links through the service, as the GUI would.
    pub(super) async fn link(
        &self,
        thread: &str,
        number: u64,
    ) -> storage::pull_requests::LinkReceipt {
        let permit = self.permit(thread);
        let revision = self.state(thread).lifecycle_revision;
        self.service
            .link(thread, &format!("{REPOSITORY}#{number}"), revision, &permit)
            .await
            .unwrap()
    }

    /// Links in the database only, as another process would.
    pub(super) fn link_directly(&self, thread: &str, number: u64) {
        let revision = self.state(thread).lifecycle_revision;
        self.storage_handle
            .link_pull_request(LinkPullRequest {
                thread_id: thread.to_owned(),
                key: key(number),
                expected_revision: revision,
                at_ns: 1,
            })
            .unwrap();
    }

    pub(super) fn marker(&self) {
        self.bus.emit(Event::new(PullRequestEvent::Changed {
            thread_id: MARKER_THREAD.into(),
            link_revision: 0,
            transition_seq: 0,
            pull_request: Inner::reference(&key(1)),
            fields: Vec::new(),
            notifications: Vec::new(),
        }));
    }

    /// Every pull request event published so far, once the service is quiet.
    pub(super) async fn quiet_events(&mut self) -> Vec<PullRequestEvent> {
        settle().await;
        self.marker();
        let mut events = Vec::new();
        loop {
            let event = self.events.recv().await.unwrap();
            if let EventKind::PullRequest(event) = event.kind {
                if event.thread_id() == MARKER_THREAD {
                    return events;
                }
                events.push(event);
            }
        }
    }

    /// Every event of any kind published so far, once the service is quiet.
    pub(super) async fn quiet_raw_events(&mut self) -> Vec<Event> {
        settle().await;
        self.marker();
        let mut events = Vec::new();
        loop {
            let event = self.events.recv().await.unwrap();
            if matches!(&event.kind, EventKind::PullRequest(e) if e.thread_id() == MARKER_THREAD) {
                return events;
            }
            events.push(event);
        }
    }

    /// The next pull request event, whatever else happens on the bus.
    pub(super) async fn next(&mut self) -> PullRequestEvent {
        loop {
            let event = self.events.recv().await.unwrap();
            if let EventKind::PullRequest(event) = event.kind {
                return event;
            }
        }
    }
}

/// Lets every task that can run, run. Under a paused clock and a single thread this reaches
/// quiescence: no step of the service waits for anything but another task.
pub(super) async fn settle() {
    for _ in 0..200 {
        tokio::task::yield_now().await;
    }
}

/// Moves virtual time forward and lets the service react.
pub(super) async fn pass(seconds: u64) {
    tokio::time::advance(Duration::from_secs(seconds)).await;
    settle().await;
}

pub(super) fn only_changed(events: &[PullRequestEvent]) -> Vec<&PullRequestEvent> {
    events
        .iter()
        .filter(|event| matches!(event, PullRequestEvent::Changed { .. }))
        .collect()
}
