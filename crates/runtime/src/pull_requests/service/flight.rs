//! One fetch of a pull request for everyone in this process who tracks it: start, the result,
//! the commit, and the delivery to the threads that are still entitled to it.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use event_bus::{Event, PullRequestChangedField, PullRequestEvent, PullRequestFetchStatus};
use storage::StorageError;
use storage::pull_requests::{
    InheritanceResolution, InheritanceState, NotificationKey, PullRequestFacts, PullRequestKey,
    PullRequestRejection, PullRequestSnapshot,
};
use tokio::sync::watch;
use tokio::time::Instant;

use super::{Inner, KeyState, PullRequestError, Stage};
use crate::ownership::OwnerPermit;
use crate::pull_requests::checkout::{CheckoutMiss, CheckoutResolution};
use crate::pull_requests::diff::{self, FactsChange};
use crate::pull_requests::ownership::permit_holds;
use crate::pull_requests::{FetchError, FetchedPullRequest};

/// How long one look for a thread's checkout may take.
const CHECKOUT_BUDGET: Duration = Duration::from_secs(5);

/// A thread that tracks the pull request, as it was when the fetch was asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    pub(crate) thread_id: String,
    pub(crate) key: PullRequestKey,
    pub(crate) link_id: u64,
    pub(crate) link_revision: u64,
    pub(crate) permit: OwnerPermit,
}

/// Who a fetch is for. The fetch is committed only if some interested party still holds its
/// thread when the answer arrives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Interest {
    /// A thread that tracks the pull request: it gets the events and the local HEAD.
    Thread(Target),
    /// A derived thread waiting to learn whether the pull request is open.
    Child { child: String, permit: OwnerPermit },
}

impl Interest {
    fn permit(&self) -> &OwnerPermit {
        match self {
            Self::Thread(target) => &target.permit,
            Self::Child { permit, .. } => permit,
        }
    }
}

/// How a fetch ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FlightOutcome {
    /// The answer was stored (or an even newer one already was).
    Committed {
        seq: u64,
        /// The stored facts call for a quick next fetch.
        pending: bool,
        /// The stored facts are merged or closed.
        terminal: bool,
        /// Storage took the facts of this fetch as the newest. Otherwise an even newer success
        /// was stored first, and this result says nothing about the pull request's state.
        stored: bool,
    },
    /// The fetch failed; the failure was recorded.
    Failed { seq: u64, error: FetchError },
    /// Nobody interested holds their thread any more, so nothing was stored.
    Discarded,
    /// Storage failed.
    StorageFailed(String),
}

/// A fetch under way. Held in the service state while it runs.
pub(crate) struct Flight {
    interests: Mutex<Vec<Interest>>,
    outcome: watch::Sender<Option<FlightOutcome>>,
}

impl Flight {
    pub(crate) fn new(interests: Vec<Interest>) -> Self {
        Self {
            interests: Mutex::new(interests),
            outcome: watch::channel(None).0,
        }
    }

    pub(crate) fn join(&self, more: Vec<Interest>) {
        let mut interests = self
            .interests
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for interest in more {
            if !interests.contains(&interest) {
                interests.push(interest);
            }
        }
    }

    fn interests(&self) -> Vec<Interest> {
        self.interests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Waits for the end of the fetch. Never cancels it.
    pub(crate) async fn wait(&self) -> FlightOutcome {
        let mut receiver = self.outcome.subscribe();
        loop {
            if let Some(outcome) = receiver.borrow_and_update().clone() {
                return outcome;
            }
            if receiver.changed().await.is_err() {
                return FlightOutcome::Discarded;
            }
        }
    }
}

impl Inner {
    /// Starts the fetch of `key`, or joins the one under way. At most one runs per key.
    pub(crate) fn start_or_join(
        self: &Arc<Self>,
        key: PullRequestKey,
        interests: Vec<Interest>,
    ) -> Arc<Flight> {
        let mut state = self.lock();
        if let Some(flight) = state.flights.get(&key) {
            flight.join(interests);
            return Arc::clone(flight);
        }
        let flight = Arc::new(Flight::new(interests));
        state.flights.insert(key.clone(), Arc::clone(&flight));
        drop(state);
        tokio::spawn(run(Arc::clone(self), key, Arc::clone(&flight)));
        flight
    }

    /// Fetches `key`, stores what came back and tells the threads. The answer is used only
    /// while some party it is for still owns its thread.
    async fn fetch_and_commit(&self, key: &PullRequestKey, flight: &Flight) -> FlightOutcome {
        let storage = self.storage.clone();
        let begin_key = key.clone();
        let seq = match self
            .blocking_at(Stage::BeginFetch, move || {
                storage.begin_pull_request_fetch(&begin_key)
            })
            .await
        {
            Ok(Ok(seq)) => seq,
            Ok(Err(error)) => return FlightOutcome::StorageFailed(error.to_string()),
            Err(error) => return FlightOutcome::StorageFailed(error.to_string()),
        };
        // The one place `gh` is started. Numbering the fetch waited, so what is true now is
        // looked at now, and nothing is awaited between this look and the start of the process
        // (`PullRequestSource::fetch` starts it before its first await).
        let interests = flight.interests();
        if !self.may_launch(interests.iter().map(Interest::permit)) {
            return FlightOutcome::Discarded;
        }
        let deadline = Duration::from_secs(self.config().fetch_timeout_seconds);
        let result = self.source.fetch(key, deadline).await;
        // Tracking may have been switched off while `gh` ran: nothing more is started for it.
        if !self.config().enabled {
            return FlightOutcome::Discarded;
        }
        let live = self.live_interests(flight.interests()).await;
        if live.is_empty() {
            return FlightOutcome::Discarded;
        }
        match result {
            Ok(fetched) => self.commit_success(key, seq, fetched, &live).await,
            Err(error) => self.commit_failure(key, seq, error, &live).await,
        }
    }

    async fn live_interests(&self, interests: Vec<Interest>) -> Vec<Interest> {
        self.blocking(move || {
            interests
                .into_iter()
                .filter(|interest| permit_holds(interest.permit()))
                .collect()
        })
        .await
        .unwrap_or_default()
    }

    async fn previous_snapshot(&self, key: &PullRequestKey) -> Option<PullRequestSnapshot> {
        let (storage, key) = (self.storage.clone(), key.clone());
        match self
            .blocking(move || storage.pull_request_snapshot(&key))
            .await
        {
            Ok(Ok(snapshot)) => snapshot,
            Ok(Err(error)) => {
                tracing::warn!(%error, "the previous pull request snapshot could not be read");
                None
            }
            Err(error) => {
                tracing::warn!(%error, "the previous pull request snapshot could not be read");
                None
            }
        }
    }

    async fn commit_success(
        &self,
        key: &PullRequestKey,
        seq: u64,
        fetched: FetchedPullRequest,
        live: &[Interest],
    ) -> FlightOutcome {
        let previous = self.previous_snapshot(key).await;
        let previous_facts = previous.as_ref().and_then(|s| s.facts.clone());
        let previous_display = previous.as_ref().map(|s| s.display.clone());
        let recovered = previous
            .as_ref()
            .and_then(|s| s.last_attempt.as_ref())
            .is_some_and(|attempt| attempt.failure.is_some());
        let facts = fetched.facts;
        let display = fetched.display;
        let (storage, commit_key, commit_facts, commit_display) = (
            self.storage.clone(),
            key.clone(),
            facts.clone(),
            display.clone(),
        );
        let at_ns = fetched.fetched_at_ns;
        let commit = match self
            .guarded_any(permits(live), move || {
                storage.commit_pull_request_fetch_with_display(
                    &commit_key,
                    seq,
                    commit_facts,
                    commit_display,
                    at_ns,
                )
            })
            .await
        {
            Ok(Ok(commit)) => commit,
            Ok(Err(error)) => return FlightOutcome::StorageFailed(error.to_string()),
            Err(PullRequestError::NotOwner) => return FlightOutcome::Discarded,
            Err(error) => return FlightOutcome::StorageFailed(error.to_string()),
        };
        // Only facts that storage accepted as the newest describe the pull request. A refused
        // result must not be read back as "open" or "merged": the state it would claim is older.
        let stored = commit.facts_stored.then_some(&facts);
        let outcome = FlightOutcome::Committed {
            seq,
            pending: diff::is_pending(stored),
            terminal: diff::is_terminal(stored),
            stored: commit.facts_stored,
        };
        if !commit.facts_stored && !commit.attempt_recorded {
            return outcome;
        }
        let change = if commit.facts_stored {
            let mut change = diff::compare(previous_facts.as_ref(), &facts);
            // The first fetch is a baseline that reads everything again anyway.
            if previous_facts.is_some()
                && previous_display
                    .as_ref()
                    .is_some_and(|previous| diff::display_changed(previous, &display))
            {
                change.fields.push(PullRequestChangedField::Display);
            }
            change
        } else {
            FactsChange::default()
        };
        let recovered = recovered && commit.attempt_recorded;
        for interest in live {
            if let Interest::Thread(target) = interest {
                self.deliver_success(
                    key,
                    target,
                    &facts,
                    &change,
                    recovered,
                    commit.facts_stored.then_some(seq),
                )
                .await;
            }
        }
        outcome
    }

    async fn commit_failure(
        &self,
        key: &PullRequestKey,
        seq: u64,
        error: FetchError,
        live: &[Interest],
    ) -> FlightOutcome {
        let previous = self.previous_snapshot(key).await;
        let previous_kind = previous
            .as_ref()
            .and_then(|s| s.last_attempt.as_ref())
            .and_then(|attempt| attempt.failure.as_ref())
            .map(|failure| failure.kind);
        let failure = error.failure();
        let kind = failure.kind;
        let (storage, fail_key) = (self.storage.clone(), key.clone());
        let at_ns = self.clock.now_ns();
        let commit = match self
            .guarded_any(permits(live), move || {
                storage.fail_pull_request_fetch(&fail_key, seq, failure, at_ns)
            })
            .await
        {
            Ok(Ok(commit)) => commit,
            Ok(Err(storage_error)) => {
                return FlightOutcome::StorageFailed(storage_error.to_string());
            }
            Err(PullRequestError::NotOwner) => return FlightOutcome::Discarded,
            Err(storage_error) => return FlightOutcome::StorageFailed(storage_error.to_string()),
        };
        if commit.attempt_recorded && previous_kind != Some(kind) {
            for interest in live {
                if let Interest::Thread(target) = interest {
                    self.deliver(
                        key,
                        target,
                        Vec::new(),
                        Vec::new(),
                        Some(PullRequestFetchStatus::Failing(kind.as_str().to_owned())),
                    )
                    .await;
                }
            }
        }
        FlightOutcome::Failed { seq, error }
    }

    async fn deliver_success(
        &self,
        key: &PullRequestKey,
        target: &Target,
        facts: &PullRequestFacts,
        change: &FactsChange,
        recovered: bool,
        adopted_seq: Option<u64>,
    ) {
        let mut fields = change.fields.clone();
        if self
            .record_local_head(key, target, facts, adopted_seq)
            .await
        {
            fields.push(PullRequestChangedField::LocalHead);
        }
        let status = recovered.then_some(PullRequestFetchStatus::Recovered);
        self.deliver(
            key,
            target,
            fields,
            change
                .notifications
                .iter()
                .map(|kind| (*kind).to_owned())
                .collect(),
            status,
        )
        .await;
    }

    /// Looks for the checkout of the pull request's head branch among the directories the
    /// thread can reach, and keeps the thread's stored HEAD true to what it finds: a new commit is
    /// stored, and when no single checkout can be named any more the old commit is forgotten (an
    /// old commit must not stand for a checkout that is gone, moved or ambiguous). Only for a
    /// thread whose link is still the one the fetch was made for, and only while tracking is on.
    /// `true` when the stored observation changed.
    async fn record_local_head(
        &self,
        key: &PullRequestKey,
        target: &Target,
        facts: &PullRequestFacts,
        adopted_seq: Option<u64>,
    ) -> bool {
        let Ok(state) = self.thread_state(&target.thread_id).await else {
            return false;
        };
        if !still_linked(&state, key, target) || !self.config().enabled {
            return false;
        }
        let (environment, thread) = (Arc::clone(&self.env), target.thread_id.clone());
        let Ok(directories) = self
            .blocking(move || environment.checkout_directories(&thread))
            .await
        else {
            return false;
        };
        let (repository, branch) = (facts.head_repository.clone(), facts.head_branch.clone());
        let resolution = if directories.is_empty() {
            CheckoutResolution::Missing(CheckoutMiss::NoCheckout)
        } else {
            // The one place `git` is started. The look at tracking and at the thread's lease is
            // the first thing the blocking closure does, immediately before the process, so
            // neither the lookup of the directories nor the wait for the pool can outdate it.
            let (environment, ownership, settings, permit) = (
                Arc::clone(&self.env),
                Arc::clone(&self.ownership),
                self.settings.subscribe(),
                target.permit.clone(),
            );
            let resolved = self
                .blocking_at(Stage::Checkout, move || {
                    // The lease is confirmed first (that may wait on a busy registry) and tracking
                    // is read after it, immediately before the process.
                    if !ownership.confirms(&permit) || !settings.borrow().enabled {
                        return None;
                    }
                    Some(environment.resolve_checkout(
                        &directories,
                        repository.as_deref(),
                        &branch,
                        CHECKOUT_BUDGET,
                    ))
                })
                .await;
            match resolved {
                Ok(Some(resolution)) => resolution,
                Ok(None) | Err(_) => return false,
            }
        };
        if let Some(seq) = adopted_seq {
            let local = match &resolution {
                CheckoutResolution::Found(checkout) => Some(checkout.head_sha.as_str()),
                CheckoutResolution::Missing(_) => None,
            };
            if self
                .check_permit(&target.thread_id, &target.permit)
                .await
                .is_ok()
            {
                self.lock().attribution.adopt(
                    &target.thread_id,
                    target.permit.lease.generation,
                    target.link_revision,
                    seq,
                    &facts.head_sha,
                    local,
                );
                self.wake.notify_one();
            }
        }
        let (storage, thread, revision) = (
            self.storage.clone(),
            target.thread_id.clone(),
            target.link_revision,
        );
        match resolution {
            CheckoutResolution::Found(checkout) => {
                if state
                    .local_head
                    .as_ref()
                    .is_some_and(|head| head.head_sha == checkout.head_sha)
                {
                    return false;
                }
                let at_ns = self.clock.now_ns();
                matches!(
                    self.guarded(&target.permit, move || {
                        storage.record_pull_request_local_head(
                            &thread,
                            revision,
                            &checkout.head_sha,
                            at_ns,
                        )
                    })
                    .await,
                    Ok(Ok(true))
                )
            }
            CheckoutResolution::Missing(_) => {
                if state.local_head.is_none() {
                    return false;
                }
                matches!(
                    self.guarded(&target.permit, move || {
                        storage.clear_pull_request_local_head(&thread, revision)
                    })
                    .await,
                    Ok(Ok(true))
                )
            }
        }
    }

    /// Publishes one change for one thread, inside one guarded write for the thread: takes the
    /// next transition number of the link it was made for (refused once the link ended), claims
    /// each notification kind exactly once, and emits once all of that is stored. The thread's
    /// generation is held fixed throughout, so a process that lost the thread cannot publish.
    async fn deliver(
        &self,
        key: &PullRequestKey,
        target: &Target,
        fields: Vec<PullRequestChangedField>,
        notifications: Vec<String>,
        status: Option<PullRequestFetchStatus>,
    ) {
        if fields.is_empty() && status.is_none() {
            return;
        }
        let (storage, bus, thread, revision) = (
            self.storage.clone(),
            Arc::clone(&self.bus),
            target.thread_id.clone(),
            target.link_revision,
        );
        let pull_request = Self::reference(key);
        let published = self
            .guarded(&target.permit, move || -> Result<(), StorageError> {
                let seq = storage.next_pull_request_transition(&thread, revision)?;
                let mut claimed = Vec::new();
                for kind in notifications {
                    let notification = NotificationKey {
                        link_revision: revision,
                        transition_seq: seq,
                        kind: kind.clone(),
                    };
                    if let Ok(true) = storage.claim_pull_request_notification(&thread, notification)
                    {
                        claimed.push(kind);
                    }
                }
                if let Some(status) = status {
                    bus.emit(Event::new(PullRequestEvent::FetchStatusChanged {
                        thread_id: thread.clone(),
                        link_revision: revision,
                        transition_seq: seq,
                        pull_request: pull_request.clone(),
                        status,
                    }));
                }
                if !fields.is_empty() {
                    bus.emit(Event::new(PullRequestEvent::Changed {
                        thread_id: thread,
                        link_revision: revision,
                        transition_seq: seq,
                        pull_request,
                        fields,
                        notifications: claimed,
                    }));
                }
                Ok(())
            })
            .await;
        match published {
            Ok(Ok(())) => {}
            Ok(Err(
                StorageError::Tombstoned { .. }
                | StorageError::PullRequest(
                    PullRequestRejection::NotLinked | PullRequestRejection::LinkNotActive,
                ),
            ))
            | Err(PullRequestError::NotOwner) => {}
            Ok(Err(error)) => {
                tracing::warn!(%error, thread = %target.thread_id, "a pull request change was not published");
            }
            Err(error) => {
                tracing::warn!(%error, thread = %target.thread_id, "a pull request change was not published");
            }
        }
    }

    /// Ends a flight: records when the pull request is due next, removes the flight, tells the
    /// waiters, and wakes the scheduler.
    pub(crate) fn finish(
        &self,
        key: &PullRequestKey,
        flight: &Arc<Flight>,
        outcome: FlightOutcome,
    ) {
        let config = self.config();
        let now = Instant::now();
        {
            let mut state = self.lock();
            if state
                .flights
                .get(key)
                .is_some_and(|current| Arc::ptr_eq(current, flight))
            {
                state.flights.remove(key);
            }
            let entry = state
                .keys
                .entry(key.clone())
                .or_insert_with(|| KeyState::new(now, false, 0));
            let pending = Duration::from_secs(config.pending_poll_seconds);
            let stable = Duration::from_secs(config.stable_poll_seconds);
            let backoff_max = Duration::from_secs(config.backoff_max_seconds);
            match &outcome {
                FlightOutcome::Committed {
                    seq,
                    pending: is_pending,
                    terminal,
                    stored,
                } => {
                    entry.failures = 0;
                    entry.backoff_until = None;
                    if *stored && *seq > entry.adopted_seq {
                        entry.adopted_seq = *seq;
                        entry.terminal = *terminal;
                        entry.next_due = if *terminal {
                            None
                        } else {
                            Some(now + if *is_pending { pending } else { stable })
                        };
                    } else {
                        // Storage kept an even newer success, or a newer view was adopted while
                        // this fetch ran: this result is not the state. The entry keeps what it
                        // has, and the next plan reads the newest facts.
                        entry.next_due = (!entry.terminal).then_some(now + pending);
                    }
                }
                FlightOutcome::Failed { error, .. } => {
                    entry.failures = entry.failures.saturating_add(1);
                    let retry = now + backoff(error, pending, backoff_max, entry.failures);
                    entry.backoff_until = Some(retry);
                    // A pull request already known to be done stays left alone: the failure of
                    // a refresh does not turn the polling back on.
                    entry.next_due = (!entry.terminal).then_some(retry);
                }
                FlightOutcome::StorageFailed(_) => {
                    entry.failures = entry.failures.saturating_add(1);
                    let retry = now + backoff_max.min(pending * 2);
                    entry.backoff_until = Some(retry);
                    entry.next_due = (!entry.terminal).then_some(retry);
                }
                FlightOutcome::Discarded => {
                    // Nobody who waited could be confirmed (or tracking was switched off):
                    // try again later, not in a tight loop against a busy registry.
                    entry.next_due = (!entry.terminal).then_some(now + pending);
                }
            }
        }
        flight.outcome.send_replace(Some(outcome));
        self.wake.notify_one();
    }

    /// Applies the pending inheritance of `child`: a fetch confirms the pull request, then
    /// storage decides.
    pub(crate) async fn resolve_child(
        self: &Arc<Self>,
        child: String,
        permit: OwnerPermit,
    ) -> Result<InheritanceResolution, PullRequestError> {
        let (storage, thread) = (self.storage.clone(), child.clone());
        let Some(record) = self
            .blocking(move || storage.pull_request_inheritance(&thread))
            .await??
        else {
            return Ok(InheritanceResolution::Missing);
        };
        if record.state != InheritanceState::Pending {
            return Ok(InheritanceResolution::AlreadyDecided(record.state));
        }
        let flight = self.start_or_join(
            record.key.clone(),
            vec![Interest::Child {
                child: child.clone(),
                permit: permit.clone(),
            }],
        );
        self.settle_child(child, permit, &flight).await
    }

    /// [`Self::resolve_child`] for a fetch the scheduler already started with the child as one of
    /// its interests, so one fetch serves the pull request's threads and its waiting children
    /// even when it ends before this task first runs.
    pub(crate) async fn resolve_child_in(
        self: &Arc<Self>,
        child: String,
        permit: OwnerPermit,
        flight: Arc<Flight>,
    ) -> Result<InheritanceResolution, PullRequestError> {
        self.settle_child(child, permit, &flight).await
    }

    async fn settle_child(
        self: &Arc<Self>,
        child: String,
        permit: OwnerPermit,
        flight: &Flight,
    ) -> Result<InheritanceResolution, PullRequestError> {
        match flight.wait().await {
            FlightOutcome::Committed { seq, .. } => {
                // The answer is shared, so it says nothing about the child's own authority:
                // the decision is written under the child's lease.
                let (storage, bus, thread, at_ns) = (
                    self.storage.clone(),
                    Arc::clone(&self.bus),
                    child.clone(),
                    self.clock.now_ns(),
                );
                let resolution = self
                    .guarded(
                        &permit,
                        move || -> Result<InheritanceResolution, StorageError> {
                            let resolution =
                                storage.resolve_pull_request_inheritance(&thread, seq, at_ns)?;
                            if let InheritanceResolution::Applied { link, .. } = &resolution {
                                let seq = storage
                                    .next_pull_request_transition(&thread, link.link_revision)?;
                                bus.emit(Event::new(PullRequestEvent::Linked {
                                    thread_id: thread,
                                    link_revision: link.link_revision,
                                    transition_seq: seq,
                                    pull_request: Self::reference(&link.key),
                                    origin: event_bus::PullRequestLinkOrigin::Inherited,
                                }));
                            }
                            Ok(resolution)
                        },
                    )
                    .await??;
                if matches!(resolution, InheritanceResolution::Applied { .. }) {
                    self.force(&child);
                }
                Ok(resolution)
            }
            FlightOutcome::Failed { error, .. } => {
                let (storage, thread, at_ns) = (self.storage.clone(), child, self.clock.now_ns());
                let note = error.failure().message;
                let _ = self
                    .guarded(&permit, move || {
                        storage.record_pull_request_inheritance_attempt(&thread, &note, at_ns)
                    })
                    .await;
                Err(PullRequestError::Fetch(error))
            }
            FlightOutcome::Discarded => Err(PullRequestError::OwnershipLost),
            FlightOutcome::StorageFailed(message) => {
                Err(PullRequestError::Storage(StorageError::Io(message)))
            }
        }
    }
}

fn permits(live: &[Interest]) -> Vec<OwnerPermit> {
    live.iter()
        .map(|interest| interest.permit().clone())
        .collect()
}

fn still_linked(
    state: &storage::pull_requests::ThreadPullRequestState,
    key: &PullRequestKey,
    target: &Target,
) -> bool {
    state.active_link.as_ref().is_some_and(|link| {
        link.link_id == target.link_id
            && link.link_revision == target.link_revision
            && link.key == *key
    })
}

/// The wait after `failures` failed fetches in a row: doubling from the quick interval up to the
/// configured maximum. A failure that retrying will not fix waits the maximum at once.
fn backoff(error: &FetchError, base: Duration, max: Duration, failures: u32) -> Duration {
    if !error.is_transient() {
        return max;
    }
    let factor = 1_u32.checked_shl(failures.min(20)).unwrap_or(u32::MAX);
    base.checked_mul(factor).map_or(max, |delay| delay.min(max))
}

async fn run(inner: Arc<Inner>, key: PullRequestKey, flight: Arc<Flight>) {
    let outcome = inner.fetch_and_commit(&key, &flight).await;
    inner.finish(&key, &flight, outcome);
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given: failures in a row / When: the wait is computed / Then: it doubles from the quick
    // interval, is capped, and a permanent failure waits the cap at once
    #[test]
    fn backoff_doubles_up_to_the_cap() {
        let (base, max) = (Duration::from_secs(30), Duration::from_secs(1_800));
        let transient = FetchError::Network("down".into());

        let waits: Vec<u64> = (1..=8)
            .map(|n| backoff(&transient, base, max, n).as_secs())
            .collect();

        assert_eq!(waits, vec![60, 120, 240, 480, 960, 1_800, 1_800, 1_800]);
        assert_eq!(
            backoff(&FetchError::GhNotFound, base, max, 1),
            max,
            "a missing gh will not appear by waiting briefly"
        );
        assert_eq!(backoff(&transient, base, max, u32::MAX), max);
    }
}
