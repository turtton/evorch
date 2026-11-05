//! Tracking the pull requests of threads (ADR 0043 Phase 1, PR3).
//!
//! [`PullRequestService`] is what the hosts call (the GUI through host commands, the review
//! through the accessors) and what keeps the stored facts current:
//!
//! - **Explicit decisions** — [`link`](PullRequestService::link),
//!   [`unlink`](PullRequestService::unlink), [`refresh`](PullRequestService::refresh), and the
//!   candidate decisions. Each takes the caller's [`OwnerPermit`]: only the process that holds
//!   a thread's lease acts for it, so a thread shown in two windows is fetched by one of them.
//! - **Tracking** — a scheduler task polls the pull requests of the threads this process owns:
//!   quickly while checks run, slowly once they settled, with backoff on failure, and not at
//!   all once the pull request is merged or closed (until a refresh finds it open again). It
//!   also wakes for the end of a `git push` or `gh pr …` command.
//! - **Candidates** — an observer reads command output for pull request URLs and offers them
//!   to the thread. It changes no tool result and no history, and fetches and links nothing.
//! - **Inheritance** — a derived thread (an escalation, later a fork or rewind) takes its
//!   parent's link only while the pull request is confirmed open.
//!
//! Nothing here talks to an agent: no notice, no held message, no wake. Changes leave as
//! [`PullRequestEvent`]s on the bus, once per change and after the change is stored; a reader
//! opens the database again instead of trusting the event.
//!
//! Every fetch is single-flight per pull request inside a process, holds no lock while `gh`
//! runs, and is never cancelled by an Unlink: it ends in the snapshot for whoever else tracks
//! the pull request. A result is delivered to a thread only if the process still owns the
//! thread (checked at completion) and the thread still has the link the fetch was started for.

mod flight;
mod observer;
mod scheduler;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use config::PullRequestsConfig;
use event_bus::{Event, EventBus, PullRequestEvent, PullRequestRef};
use storage::pull_requests::{
    CandidateState, InheritanceRecord, InheritanceResolution, LinkPullRequest, LinkReceipt,
    PullRequestCandidate, PullRequestKey, PullRequestRejection, UnlinkReceipt,
};
use storage::{StorageError, StorageHandle};
use tokio::sync::{Notify, watch};
use tokio::time::Instant;

use super::locator::{LocatorError, parse_locator};
use super::ownership::{OwnershipView, ThreadEnvironment, permit_holds};
use super::review::{
    Freshness, ReviewLookup, ReviewPullRequest, ReviewRefresh, ReviewSupply, StaleReason,
};
use super::{FetchError, PullRequestSource};
use crate::ownership::OwnerPermit;

pub(crate) use flight::{Flight, FlightOutcome, Interest, Target};

/// Time, as the database records it.
pub trait WallClock: Send + Sync {
    /// Nanoseconds since the Unix epoch.
    fn now_ns(&self) -> i64;
}

/// The system clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemWallClock;

impl WallClock for SystemWallClock {
    fn now_ns(&self) -> i64 {
        crate::run_store::review_time_ns()
    }
}

/// Why a service call failed.
#[derive(Debug, thiserror::Error)]
pub enum PullRequestError {
    /// `[pull_requests] enabled` is false.
    #[error("pull request tracking is disabled")]
    Disabled,
    /// The reference is not a github.com pull request.
    #[error("{0}")]
    Locator(#[from] LocatorError),
    /// This process does not hold the thread's lease, or the permit is for another thread.
    #[error("this process does not own the thread")]
    NotOwner,
    /// The thread changed since the caller read it (a later link or unlink won).
    #[error("the thread's pull request changed since it was read")]
    StaleRevision,
    /// The thread has no active link.
    #[error("the thread has no active pull request link")]
    NotLinked,
    /// The candidate does not exist or was already decided.
    #[error("pull request candidate not found")]
    CandidateNotFound,
    /// The thread was deleted.
    #[error("the thread was deleted")]
    Deleted,
    /// Storage refused the call.
    #[error("{0}")]
    Rejected(PullRequestRejection),
    /// Storage failed.
    #[error("storage: {0}")]
    Storage(StorageError),
    /// The fetch failed.
    #[error("fetch: {0}")]
    Fetch(FetchError),
    /// Ownership was lost while the fetch ran, so its result was dropped.
    #[error("ownership was lost before the result could be used")]
    OwnershipLost,
    /// The ownership registry stayed busy, so the lease could not be checked. Nothing was
    /// done; try again.
    #[error("the ownership registry is busy")]
    OwnershipBusy,
}

impl From<StorageError> for PullRequestError {
    fn from(error: StorageError) -> Self {
        match error {
            StorageError::StaleRevision { .. } => Self::StaleRevision,
            StorageError::Tombstoned { .. } => Self::Deleted,
            StorageError::PullRequest(PullRequestRejection::NotLinked) => Self::NotLinked,
            StorageError::PullRequest(PullRequestRejection::CandidateNotFound) => {
                Self::CandidateNotFound
            }
            StorageError::PullRequest(rejection) => Self::Rejected(rejection),
            other => Self::Storage(other),
        }
    }
}

/// How a thread was derived from another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DerivedThreadKind {
    /// An escalation to a new orchestrator.
    Escalation,
    /// A fork of a conversation.
    Fork,
    /// A rewind of a conversation.
    Rewind,
    /// A lane: it never inherits.
    Lane,
}

/// What scheduling an inheritance did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InheritanceScheduling {
    /// Pull request tracking is off.
    Disabled,
    /// The parent has no active link, so there is nothing to inherit.
    NoParentLink,
    /// A lane thread never inherits.
    Lane,
    /// The inheritance is stored (or already was) and will be resolved by the child's owner.
    Scheduled(Box<InheritanceRecord>),
}

/// Everything the service is built from.
pub struct ServiceParts {
    /// Where the rows live.
    pub storage: StorageHandle,
    /// How facts are fetched.
    pub source: Arc<dyn PullRequestSource>,
    /// Who owns which thread.
    pub ownership: Arc<dyn OwnershipView>,
    /// Facts about threads.
    pub environment: Arc<dyn ThreadEnvironment>,
    /// Where events are published and tool output is observed.
    pub bus: Arc<EventBus>,
    /// The settings at start; see [`PullRequestService::update_settings`].
    pub settings: PullRequestsConfig,
    /// Time.
    pub clock: Arc<dyn WallClock>,
}

/// Per-key polling state kept in this process.
#[derive(Debug)]
pub(crate) struct KeyState {
    /// When the tracking threads fetch it next; `None` while it is parked.
    pub(crate) next_due: Option<Instant>,
    /// The newest stored facts say merged or closed: the tracking threads leave it alone. This is
    /// kept apart from `next_due` so that a failed fetch (an explicit refresh that did not get
    /// through) cannot start the polling again. Only facts that show it open end it.
    pub(crate) terminal: bool,
    /// Failed fetches since the last success.
    pub(crate) failures: u32,
    /// Until when a failed fetch is not tried again by whoever waits for the pull request (a
    /// pending inheritance, the tracking threads). Independent of `terminal`.
    pub(crate) backoff_until: Option<Instant>,
    /// The `success_seq` of the newest facts this entry has adopted (0: none yet). Whether the
    /// stored facts are merged or open is taken from them only when they are newer than this, so
    /// neither the order a plan and a finished fetch happen to run in nor a fetch that storage
    /// refused as older can turn the entry back to an older view.
    pub(crate) adopted_seq: u64,
}

impl KeyState {
    pub(crate) fn new(now: Instant, terminal: bool, adopted_seq: u64) -> Self {
        Self {
            next_due: if terminal { None } else { Some(now) },
            terminal,
            failures: 0,
            backoff_until: None,
            adopted_seq,
        }
    }
}

#[derive(Default)]
pub(crate) struct State {
    pub(crate) keys: HashMap<PullRequestKey, KeyState>,
    pub(crate) flights: HashMap<PullRequestKey, Arc<Flight>>,
    /// Threads a person or a finished command asked to fetch for now.
    pub(crate) forced: HashSet<String>,
    /// Children whose inheritance is being resolved.
    pub(crate) resolving: HashSet<String>,
    /// When a pending inheritance is tried again.
    pub(crate) inheritance_due: HashMap<String, Instant>,
}

/// A wait on the blocking pool right before a process is started. A test may interrupt it, which
/// is how "the world changed while the work was queued" is simulated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Stage {
    /// Numbering a fetch, right before `gh` is started.
    BeginFetch,
    /// The look for a checkout, right before `git` is started.
    Checkout,
    /// The scheduler has read its plan and has not yet applied it.
    Planned,
}

pub(crate) struct Inner {
    pub(crate) storage: StorageHandle,
    pub(crate) source: Arc<dyn PullRequestSource>,
    pub(crate) ownership: Arc<dyn OwnershipView>,
    pub(crate) env: Arc<dyn ThreadEnvironment>,
    pub(crate) bus: Arc<EventBus>,
    pub(crate) clock: Arc<dyn WallClock>,
    pub(crate) settings: watch::Sender<PullRequestsConfig>,
    pub(crate) wake: Notify,
    pub(crate) state: Mutex<State>,
    pub(crate) shutdown: watch::Sender<bool>,
    #[cfg(test)]
    pub(crate) stage_hooks: Mutex<HashMap<Stage, Box<dyn FnOnce() + Send>>>,
}

impl Inner {
    pub(crate) fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Runs blocking work (storage and registry calls, git) off the async threads. Tests run it
    /// inline: a paused clock must not advance while such work is outstanding.
    pub(crate) async fn blocking<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, PullRequestError> {
        #[cfg(test)]
        {
            Ok(work())
        }
        #[cfg(not(test))]
        {
            tokio::task::spawn_blocking(work)
                .await
                .map_err(|error| PullRequestError::Storage(StorageError::Io(error.to_string())))
        }
    }

    /// [`Self::blocking`] that a test can interrupt at `stage` before the work runs.
    #[cfg(test)]
    pub(crate) async fn blocking_at<T: Send + 'static>(
        &self,
        stage: Stage,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, PullRequestError> {
        self.run_stage_hook(stage);
        self.blocking(work).await
    }

    /// Runs the hook a test registered for `stage`, once.
    #[cfg(test)]
    pub(crate) fn run_stage_hook(&self, stage: Stage) {
        let hook = self.stage_hooks.lock().unwrap().remove(&stage);
        if let Some(hook) = hook {
            hook();
        }
    }

    #[cfg(not(test))]
    pub(crate) fn run_stage_hook(&self, _stage: Stage) {}

    #[cfg(not(test))]
    pub(crate) async fn blocking_at<T: Send + 'static>(
        &self,
        _stage: Stage,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, PullRequestError> {
        self.blocking(work).await
    }

    pub(crate) fn config(&self) -> PullRequestsConfig {
        *self.settings.borrow()
    }

    /// The last look before `gh` is started: tracking is on *now*, and some party that waits for
    /// the answer holds its thread *now*. It is synchronous and does not wait for the registry
    /// (a busy registry means "not confirmed"), so nothing can happen between it and the start
    /// of the process; callers must not await between this and the launch.
    pub(crate) fn may_launch<'a>(
        &self,
        permits: impl IntoIterator<Item = &'a OwnerPermit>,
    ) -> bool {
        // The permits first, tracking last: whatever the first took time over, the last look is
        // the freshest.
        permits
            .into_iter()
            .any(|permit| matches!(permit.try_mutation_guard(), Ok(Some(_))))
            && self.config().enabled
    }

    pub(crate) fn enabled(&self) -> Result<PullRequestsConfig, PullRequestError> {
        let config = self.config();
        if config.enabled {
            Ok(config)
        } else {
            Err(PullRequestError::Disabled)
        }
    }

    pub(crate) fn emit(&self, event: PullRequestEvent) {
        self.bus.emit(Event::new(event));
    }

    pub(crate) fn force(&self, thread_id: &str) {
        self.lock().forced.insert(thread_id.to_owned());
        self.wake.notify_one();
    }

    /// The caller must hold `thread_id`'s lease right now.
    pub(crate) async fn check_permit(
        &self,
        thread_id: &str,
        permit: &OwnerPermit,
    ) -> Result<(), PullRequestError> {
        if permit.thread_id != thread_id {
            return Err(PullRequestError::NotOwner);
        }
        let permit = permit.clone();
        if self.blocking(move || permit_holds(&permit)).await? {
            Ok(())
        } else {
            Err(PullRequestError::NotOwner)
        }
    }

    /// Pins the parent's active link for `child_thread_id`. Blocks on the database.
    pub(crate) fn pin_inheritance(
        &self,
        parent_thread_id: &str,
        child_thread_id: &str,
        kind: DerivedThreadKind,
    ) -> Result<InheritanceScheduling, PullRequestError> {
        if self.enabled().is_err() {
            return Ok(InheritanceScheduling::Disabled);
        }
        if kind == DerivedThreadKind::Lane {
            return Ok(InheritanceScheduling::Lane);
        }
        // One transaction reads the parent's link and stores the row, so a parent that relinks at
        // this moment is pinned to its old link or its new one, never to neither.
        let pinned = self.storage.pin_pull_request_inheritance(
            child_thread_id,
            parent_thread_id,
            self.clock.now_ns(),
        );
        let record = match pinned {
            Ok(Some(record)) => record,
            Ok(None) => return Ok(InheritanceScheduling::NoParentLink),
            Err(StorageError::PullRequest(PullRequestRejection::LaneThread)) => {
                return Ok(InheritanceScheduling::Lane);
            }
            Err(error) => return Err(error.into()),
        };
        self.wake.notify_one();
        Ok(InheritanceScheduling::Scheduled(Box::new(record)))
    }

    pub(crate) fn reference(key: &PullRequestKey) -> PullRequestRef {
        PullRequestRef {
            provider: key.provider.as_str().to_owned(),
            host: key.host.clone(),
            repository: key.repository.clone(),
            number: key.number,
        }
    }

    /// Runs a short write for `permit`'s thread while its generation is held fixed by a registry
    /// guard (ADR 0024): a claim or handoff cannot commit until the write and the event that
    /// announces it are done. A check made earlier proves nothing about the write, so every write
    /// that acts for a thread goes through here. Never hold it across a wait for `gh` or `git`.
    pub(crate) async fn guarded<T: Send + 'static>(
        &self,
        permit: &OwnerPermit,
        write: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, PullRequestError> {
        self.guarded_any(vec![permit.clone()], write).await
    }

    /// [`Self::guarded`] under the first of `permits` that still holds its thread, for a write
    /// that serves several threads (the snapshot of a shared pull request).
    pub(crate) async fn guarded_any<T: Send + 'static>(
        &self,
        permits: Vec<OwnerPermit>,
        write: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, PullRequestError> {
        self.blocking(move || {
            let mut refusal = PullRequestError::NotOwner;
            for permit in &permits {
                match permit.mutation_guard() {
                    Ok(guard) => {
                        let written = write();
                        drop(guard);
                        return Ok(written);
                    }
                    Err(error) if error.is_fenced() => refusal = PullRequestError::NotOwner,
                    Err(_) => refusal = PullRequestError::OwnershipBusy,
                }
            }
            Err(refusal)
        })
        .await?
    }

    pub(crate) async fn link_key(
        &self,
        thread_id: &str,
        key: PullRequestKey,
        expected_revision: u64,
        permit: &OwnerPermit,
    ) -> Result<LinkReceipt, PullRequestError> {
        self.enabled()?;
        self.check_permit(thread_id, permit).await?;
        let (storage, bus, thread) = (
            self.storage.clone(),
            Arc::clone(&self.bus),
            thread_id.to_owned(),
        );
        let request = LinkPullRequest {
            thread_id: thread_id.to_owned(),
            key,
            expected_revision,
            at_ns: self.clock.now_ns(),
        };
        let receipt = self
            .guarded(permit, move || -> Result<LinkReceipt, StorageError> {
                let receipt = storage.link_pull_request(request)?;
                if receipt.changed {
                    let seq = storage
                        .next_pull_request_transition(&thread, receipt.link.link_revision)?;
                    if let Some(old) = &receipt.superseded {
                        bus.emit(Event::new(PullRequestEvent::Unlinked {
                            thread_id: thread.clone(),
                            link_revision: old.link_revision,
                            transition_seq: seq,
                            pull_request: Self::reference(&old.key),
                            reason: event_bus::PullRequestLinkEnd::Relinked,
                        }));
                    }
                    bus.emit(Event::new(PullRequestEvent::Linked {
                        thread_id: thread,
                        link_revision: receipt.link.link_revision,
                        transition_seq: seq,
                        pull_request: Self::reference(&receipt.link.key),
                        origin: event_bus::PullRequestLinkOrigin::Explicit,
                    }));
                }
                Ok(receipt)
            })
            .await??;
        self.force(thread_id);
        Ok(receipt)
    }

    pub(crate) async fn thread_state(
        &self,
        thread_id: &str,
    ) -> Result<storage::pull_requests::ThreadPullRequestState, PullRequestError> {
        let (storage, thread) = (self.storage.clone(), thread_id.to_owned());
        Ok(self
            .blocking(move || storage.pull_request_thread_state(&thread))
            .await??)
    }

    async fn candidates(
        &self,
        thread_id: &str,
    ) -> Result<Vec<PullRequestCandidate>, PullRequestError> {
        let (storage, thread) = (self.storage.clone(), thread_id.to_owned());
        Ok(self
            .blocking(move || storage.pull_request_candidates(&thread, None))
            .await??)
    }

    async fn candidate_event(
        &self,
        thread_id: &str,
        candidate: &PullRequestCandidate,
        state: event_bus::PullRequestCandidateState,
    ) -> Result<(), PullRequestError> {
        let current = self.thread_state(thread_id).await?;
        self.emit(PullRequestEvent::CandidateChanged {
            thread_id: thread_id.to_owned(),
            link_revision: current.lifecycle_revision,
            transition_seq: current.transition_seq,
            pull_request: Self::reference(&candidate.key),
            candidate_id: candidate.candidate_id,
            state,
        });
        Ok(())
    }

    /// The review's view of a thread, from the database only.
    async fn review_of_thread(
        &self,
        thread_id: &str,
        config: PullRequestsConfig,
    ) -> Result<ReviewLookup, PullRequestError> {
        let state = self.thread_state(thread_id).await?;
        let Some(link) = state.active_link else {
            return Ok(ReviewLookup::NotLinked);
        };
        let (storage, key) = (self.storage.clone(), link.key.clone());
        let snapshot = self
            .blocking(move || storage.pull_request_snapshot(&key))
            .await??;
        let now_ns = self.clock.now_ns();
        let (facts, fetched_at_ns, last_failure) = match snapshot {
            Some(snapshot) => (
                snapshot.facts,
                snapshot.fetched_at_ns,
                snapshot.last_attempt.and_then(|attempt| attempt.failure),
            ),
            None => (None, None, None),
        };
        let age = fetched_at_ns
            .map(|at| Duration::from_nanos(u64::try_from(now_ns.saturating_sub(at)).unwrap_or(0)));
        let max_age = Duration::from_secs(config.review_max_age_seconds);
        let freshness = if facts.is_none() {
            Freshness::Stale(StaleReason::NoFacts)
        } else if last_failure.is_some() {
            Freshness::Stale(StaleReason::LastFetchFailed)
        } else if config.review_max_age_seconds == 0 || age.is_none_or(|age| age > max_age) {
            Freshness::Stale(StaleReason::TooOld)
        } else {
            Freshness::Fresh
        };
        Ok(ReviewLookup::Linked(Box::new(ReviewPullRequest {
            thread_id: thread_id.to_owned(),
            link,
            facts,
            fetched_at_ns,
            age,
            freshness,
            last_failure,
            local_head: state.local_head,
        })))
    }
}

struct TaskGuard(Arc<Inner>);

impl Drop for TaskGuard {
    fn drop(&mut self) {
        self.0.shutdown.send_replace(true);
    }
}

/// Tracks the pull requests of threads. Cheap to clone; the background tasks stop when the
/// last clone is dropped.
#[derive(Clone)]
pub struct PullRequestService {
    inner: Arc<Inner>,
    _guard: Arc<TaskGuard>,
}

impl PullRequestService {
    /// Builds the service and starts its tasks (the scheduler and the output observer).
    /// Must be called inside a Tokio runtime. While `settings.enabled` is false the tasks
    /// idle: nothing is fetched, no `gh` or `git` runs, and nothing is observed.
    ///
    /// # Errors
    /// The settings are outside their documented ranges.
    pub fn start(parts: ServiceParts) -> Result<Self, config::ConfigError> {
        parts.settings.validate()?;
        let (settings, _) = watch::channel(parts.settings);
        let (shutdown, _) = watch::channel(false);
        let inner = Arc::new(Inner {
            storage: parts.storage,
            source: parts.source,
            ownership: parts.ownership,
            env: parts.environment,
            bus: parts.bus,
            clock: parts.clock,
            settings,
            wake: Notify::new(),
            state: Mutex::new(State::default()),
            shutdown,
            #[cfg(test)]
            stage_hooks: Mutex::default(),
        });
        let receiver = inner.bus.subscribe();
        tokio::spawn(scheduler::run(Arc::clone(&inner)));
        tokio::spawn(observer::run(Arc::clone(&inner), receiver));
        Ok(Self {
            _guard: Arc::new(TaskGuard(Arc::clone(&inner))),
            inner,
        })
    }

    /// Replaces the settings. Takes effect at once: the scheduler re-plans, a disabled service
    /// starts no further fetch.
    ///
    /// # Errors
    /// The settings are outside their documented ranges (nothing changes).
    pub fn update_settings(&self, settings: PullRequestsConfig) -> Result<(), config::ConfigError> {
        settings.validate()?;
        self.inner.settings.send_replace(settings);
        self.inner.wake.notify_one();
        Ok(())
    }

    /// The settings in force.
    #[must_use]
    pub fn settings(&self) -> PullRequestsConfig {
        self.inner.config()
    }

    /// Tells the scheduler that something it plans from changed: a thread was archived or
    /// restored, a thread was deleted, or a thread's owner changed. Cheap and idempotent.
    pub fn notify_changed(&self) {
        self.inner.wake.notify_one();
    }

    /// Links `thread_id` to the pull request named by `locator` (a github.com URL or
    /// `owner/repo#123`). `expected_revision` is the lifecycle revision the caller read, so
    /// a decision made meanwhile is never overwritten. The pull request is fetched soon after.
    ///
    /// # Errors
    /// [`PullRequestError::Disabled`], [`PullRequestError::Locator`],
    /// [`PullRequestError::NotOwner`], [`PullRequestError::StaleRevision`], storage errors.
    pub async fn link(
        &self,
        thread_id: &str,
        locator: &str,
        expected_revision: u64,
        permit: &OwnerPermit,
    ) -> Result<LinkReceipt, PullRequestError> {
        let key = parse_locator(locator)?;
        self.inner
            .link_key(thread_id, key, expected_revision, permit)
            .await
    }

    /// Ends the thread's link. `expected_revision` is the lifecycle revision the caller read;
    /// a link made after that is never erased. The pull request stays tracked for every other
    /// thread that links it, and a fetch under way is not cancelled.
    ///
    /// # Errors
    /// As [`Self::link`], and [`PullRequestError::NotLinked`].
    pub async fn unlink(
        &self,
        thread_id: &str,
        expected_revision: u64,
        permit: &OwnerPermit,
    ) -> Result<UnlinkReceipt, PullRequestError> {
        let inner = &self.inner;
        inner.enabled()?;
        inner.check_permit(thread_id, permit).await?;
        let (storage, bus, thread, at_ns) = (
            inner.storage.clone(),
            Arc::clone(&inner.bus),
            thread_id.to_owned(),
            inner.clock.now_ns(),
        );
        let receipt = inner
            .guarded(permit, move || -> Result<UnlinkReceipt, StorageError> {
                let receipt = storage.unlink_pull_request(&thread, expected_revision, at_ns)?;
                if let Some(ended) = &receipt.ended {
                    let state = storage.pull_request_thread_state(&thread)?;
                    bus.emit(Event::new(PullRequestEvent::Unlinked {
                        thread_id: thread,
                        link_revision: ended.link_revision,
                        transition_seq: state.transition_seq,
                        pull_request: Inner::reference(&ended.key),
                        reason: event_bus::PullRequestLinkEnd::Unlinked,
                    }));
                }
                Ok(receipt)
            })
            .await??;
        inner.wake.notify_one();
        Ok(receipt)
    }

    /// Asks for a fetch of the thread's pull request now (the "update" operation). Returns once
    /// the request is registered; the result arrives as stored facts and an event. A thread
    /// whose owner is gone becomes owned by the process that got `permit` (the caller starts
    /// or claims the lease first, as it does for chat), which resumes the polling.
    ///
    /// # Errors
    /// As [`Self::link`], and [`PullRequestError::NotLinked`].
    pub async fn refresh(
        &self,
        thread_id: &str,
        permit: &OwnerPermit,
    ) -> Result<(), PullRequestError> {
        let inner = &self.inner;
        inner.enabled()?;
        inner.check_permit(thread_id, permit).await?;
        if inner.thread_state(thread_id).await?.active_link.is_none() {
            return Err(PullRequestError::NotLinked);
        }
        inner.force(thread_id);
        Ok(())
    }

    /// Ignores a candidate chip. A decided candidate is not shown again, not even after an
    /// Unlink.
    ///
    /// # Errors
    /// [`PullRequestError::Disabled`], [`PullRequestError::CandidateNotFound`], storage errors.
    pub async fn dismiss_candidate(
        &self,
        thread_id: &str,
        candidate_id: u64,
    ) -> Result<PullRequestCandidate, PullRequestError> {
        let inner = &self.inner;
        inner.enabled()?;
        let before = inner
            .candidates(thread_id)
            .await?
            .into_iter()
            .find(|candidate| candidate.candidate_id == candidate_id)
            .ok_or(PullRequestError::CandidateNotFound)?;
        let (storage, thread, at_ns) = (
            inner.storage.clone(),
            thread_id.to_owned(),
            inner.clock.now_ns(),
        );
        let after = inner
            .blocking(move || storage.dismiss_pull_request_candidate(&thread, candidate_id, at_ns))
            .await??;
        if before.state == CandidateState::Presented && after.state == CandidateState::Dismissed {
            inner
                .candidate_event(
                    thread_id,
                    &after,
                    event_bus::PullRequestCandidateState::Dismissed,
                )
                .await?;
        }
        Ok(after)
    }

    /// Links the thread to a presented candidate (one click on its chip).
    ///
    /// # Errors
    /// As [`Self::link`], and [`PullRequestError::CandidateNotFound`] for a candidate that is
    /// not presented any more.
    pub async fn confirm_candidate(
        &self,
        thread_id: &str,
        candidate_id: u64,
        expected_revision: u64,
        permit: &OwnerPermit,
    ) -> Result<LinkReceipt, PullRequestError> {
        let inner = &self.inner;
        inner.enabled()?;
        let candidate = inner
            .candidates(thread_id)
            .await?
            .into_iter()
            .find(|candidate| {
                candidate.candidate_id == candidate_id
                    && candidate.state == CandidateState::Presented
            })
            .ok_or(PullRequestError::CandidateNotFound)?;
        let receipt = inner
            .link_key(thread_id, candidate.key.clone(), expected_revision, permit)
            .await?;
        inner
            .candidate_event(
                thread_id,
                &candidate,
                event_bus::PullRequestCandidateState::Confirmed,
            )
            .await?;
        Ok(receipt)
    }

    /// Records that `child_thread_id`, derived from `parent_thread_id`, may take the parent's
    /// active link once the pull request is confirmed open. The parent's link is pinned **now**:
    /// whatever the parent does afterwards (unlink, relink) does not change which pull request the
    /// child may take. Idempotent; a lane never inherits. The child's owner resolves it (see
    /// [`Self::resolve_inheritance`]); this process does so by itself when it owns the child.
    ///
    /// # Errors
    /// Storage errors.
    pub async fn inherit(
        &self,
        parent_thread_id: &str,
        child_thread_id: &str,
        kind: DerivedThreadKind,
    ) -> Result<InheritanceScheduling, PullRequestError> {
        let (inner, parent, child) = (
            Arc::clone(&self.inner),
            parent_thread_id.to_owned(),
            child_thread_id.to_owned(),
        );
        self.inner
            .blocking(move || inner.pin_inheritance(&parent, &child, kind))
            .await?
    }

    /// [`Self::inherit`] for synchronous code that creates the child (an escalation): the link
    /// is pinned before this returns, in the path that makes the child. It blocks briefly on the
    /// database and starts no process; only the confirmation that the pull request is open is
    /// left to the background.
    ///
    /// # Errors
    /// Storage errors.
    pub fn inherit_now(
        &self,
        parent_thread_id: &str,
        child_thread_id: &str,
        kind: DerivedThreadKind,
    ) -> Result<InheritanceScheduling, PullRequestError> {
        self.inner
            .pin_inheritance(parent_thread_id, child_thread_id, kind)
    }

    /// Tries to apply the pending inheritance of `child_thread_id` now: confirms the pull
    /// request is open with a fetch, then lets storage decide (applied, skipped because it is
    /// merged or closed, or still pending). An unconfirmed inheritance stays pending.
    ///
    /// # Errors
    /// [`PullRequestError::NotOwner`] unless `permit` holds the child; [`PullRequestError::Fetch`]
    /// when the fetch failed (the attempt is counted, the inheritance stays pending).
    pub async fn resolve_inheritance(
        &self,
        child_thread_id: &str,
        permit: &OwnerPermit,
    ) -> Result<InheritanceResolution, PullRequestError> {
        self.inner.enabled()?;
        self.inner.check_permit(child_thread_id, permit).await?;
        self.inner
            .resolve_child(child_thread_id.to_owned(), permit.clone())
            .await
    }

    /// What the review reads about the thread of `run_id`. Reads the database only: it starts
    /// no `gh`, no `git` and no process. Use it when the review must not wait.
    ///
    /// # Errors
    /// Storage errors.
    pub async fn snapshot_for_review(
        &self,
        run_id: &str,
    ) -> Result<ReviewLookup, PullRequestError> {
        let config = self.inner.config();
        if !config.enabled {
            return Ok(ReviewLookup::Disabled);
        }
        if !config.include_in_review {
            return Ok(ReviewLookup::NotIncluded);
        }
        let (env, run) = (Arc::clone(&self.inner.env), run_id.to_owned());
        let Some(thread) = self.inner.blocking(move || env.thread_of_run(&run)).await? else {
            return Ok(ReviewLookup::NotLinked);
        };
        self.inner.review_of_thread(&thread, config).await
    }

    /// [`Self::snapshot_for_review`], first fetching when the stored facts are older than
    /// `review_max_age_seconds` (or failed or missing) and this process owns the thread. The
    /// wait is bounded by `review_refresh_timeout_seconds`; a fetch that runs longer goes on
    /// for the tracker, and the review proceeds with what is stored, marked stale.
    ///
    /// # Errors
    /// Storage errors.
    pub async fn refresh_for_review(&self, run_id: &str) -> Result<ReviewSupply, PullRequestError> {
        let inner = &self.inner;
        let config = inner.config();
        let lookup = self.snapshot_for_review(run_id).await?;
        let ReviewLookup::Linked(review) = &lookup else {
            return Ok(ReviewSupply {
                lookup,
                refresh: ReviewRefresh::NotNeeded,
            });
        };
        if review.freshness == Freshness::Fresh {
            return Ok(ReviewSupply {
                lookup,
                refresh: ReviewRefresh::NotNeeded,
            });
        }
        let (ownership, thread) = (Arc::clone(&inner.ownership), review.thread_id.clone());
        let Some(permit) = inner
            .blocking(move || ownership.owned_permit(&thread))
            .await?
        else {
            return Ok(ReviewSupply {
                lookup,
                refresh: ReviewRefresh::NotOwner,
            });
        };
        let target = Target {
            thread_id: review.thread_id.clone(),
            key: review.link.key.clone(),
            link_id: review.link.link_id,
            link_revision: review.link.link_revision,
            permit,
        };
        let flight = inner.start_or_join(review.link.key.clone(), vec![Interest::Thread(target)]);
        let waited = tokio::time::timeout(
            Duration::from_secs(config.review_refresh_timeout_seconds),
            flight.wait(),
        )
        .await;
        let refresh = match waited {
            Err(_) => ReviewRefresh::TimedOut,
            Ok(FlightOutcome::Committed { .. }) => ReviewRefresh::Refreshed,
            Ok(FlightOutcome::Failed { .. } | FlightOutcome::StorageFailed(_)) => {
                ReviewRefresh::FetchFailed
            }
            Ok(FlightOutcome::Discarded) => ReviewRefresh::NotOwner,
        };
        let lookup = self.snapshot_for_review(run_id).await?;
        Ok(ReviewSupply { lookup, refresh })
    }

    /// Stops the background tasks now (they also stop when the last clone is dropped).
    pub fn shutdown(&self) {
        self.inner.shutdown.send_replace(true);
    }

    #[cfg(test)]
    pub(crate) fn inner(&self) -> &Arc<Inner> {
        &self.inner
    }
}

#[cfg(test)]
mod tests;
