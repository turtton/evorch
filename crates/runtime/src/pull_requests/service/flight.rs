//! One fetch of a pull request for everyone in this process who tracks it: start, the result,
//! the commit, and the delivery to the threads that are still entitled to it.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use event_bus::{PullRequestChangedField, PullRequestEvent, PullRequestFetchStatus};
use storage::StorageError;
use storage::pull_requests::{
    InheritanceResolution, InheritanceState, NotificationKey, PullRequestFacts, PullRequestKey,
    PullRequestRejection, PullRequestSnapshot,
};
use tokio::sync::watch;
use tokio::time::Instant;

use super::{Inner, KeyState, PullRequestError};
use crate::ownership::OwnerPermit;
use crate::pull_requests::checkout::{CheckoutResolution, resolve_blocking};
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
    fn new(interests: Vec<Interest>) -> Self {
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
        let config = self.config();
        let storage = self.storage.clone();
        let begin_key = key.clone();
        let seq = match self
            .blocking(move || storage.begin_pull_request_fetch(&begin_key))
            .await
        {
            Ok(Ok(seq)) => seq,
            Ok(Err(error)) => return FlightOutcome::StorageFailed(error.to_string()),
            Err(error) => return FlightOutcome::StorageFailed(error.to_string()),
        };
        let result = self
            .source
            .fetch(key, Duration::from_secs(config.fetch_timeout_seconds))
            .await;
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
        let recovered = previous
            .as_ref()
            .and_then(|s| s.last_attempt.as_ref())
            .is_some_and(|attempt| attempt.failure.is_some());
        let facts = fetched.facts;
        let (storage, commit_key, commit_facts) =
            (self.storage.clone(), key.clone(), facts.clone());
        let at_ns = fetched.fetched_at_ns;
        let commit = match self
            .blocking(move || {
                storage.commit_pull_request_fetch(&commit_key, seq, commit_facts, at_ns)
            })
            .await
        {
            Ok(Ok(commit)) => commit,
            Ok(Err(error)) => return FlightOutcome::StorageFailed(error.to_string()),
            Err(error) => return FlightOutcome::StorageFailed(error.to_string()),
        };
        let stored = if commit.facts_stored {
            Some(&facts)
        } else {
            previous_facts.as_ref()
        };
        let outcome = FlightOutcome::Committed {
            seq,
            pending: diff::is_pending(stored),
            terminal: diff::is_terminal(stored),
        };
        if !commit.facts_stored && !commit.attempt_recorded {
            return outcome;
        }
        let change = if commit.facts_stored {
            diff::compare(previous_facts.as_ref(), &facts)
        } else {
            FactsChange::default()
        };
        let recovered = recovered && commit.attempt_recorded;
        for interest in live {
            if let Interest::Thread(target) = interest {
                self.deliver_success(key, target, &facts, &change, recovered)
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
            .blocking(move || storage.fail_pull_request_fetch(&fail_key, seq, failure, at_ns))
            .await
        {
            Ok(Ok(commit)) => commit,
            Ok(Err(storage_error)) => {
                return FlightOutcome::StorageFailed(storage_error.to_string());
            }
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
    ) {
        let mut fields = change.fields.clone();
        if self.record_local_head(key, target, facts).await {
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
    /// thread can reach and stores its HEAD for this link. Only for a thread whose link is
    /// still the one the fetch was made for. `true` when the stored observation changed.
    async fn record_local_head(
        &self,
        key: &PullRequestKey,
        target: &Target,
        facts: &PullRequestFacts,
    ) -> bool {
        let Ok(state) = self.thread_state(&target.thread_id).await else {
            return false;
        };
        if !still_linked(&state, key, target) {
            return false;
        }
        let directories = self.env.checkout_directories(&target.thread_id);
        if directories.is_empty() {
            return false;
        }
        let (repository, branch) = (facts.head_repository.clone(), facts.head_branch.clone());
        let resolution = self
            .blocking(move || {
                resolve_blocking(
                    &directories,
                    repository.as_deref(),
                    &branch,
                    CHECKOUT_BUDGET,
                )
            })
            .await;
        let Ok(CheckoutResolution::Found(checkout)) = resolution else {
            return false;
        };
        if state
            .local_head
            .as_ref()
            .is_some_and(|head| head.head_sha == checkout.head_sha)
        {
            return false;
        }
        let (storage, thread, at_ns) = (
            self.storage.clone(),
            target.thread_id.clone(),
            self.clock.now_ns(),
        );
        let revision = target.link_revision;
        matches!(
            self.blocking(move || {
                storage.record_pull_request_local_head(&thread, revision, &checkout.head_sha, at_ns)
            })
            .await,
            Ok(Ok(true))
        )
    }

    /// Publishes one change for one thread: takes the next transition number of the link it
    /// was made for (refused once the link ended), claims each notification kind exactly once,
    /// and emits after all of that is stored.
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
        let seq = match self
            .next_transition(&target.thread_id, target.link_revision)
            .await
        {
            Ok(seq) => seq,
            Err(
                PullRequestError::NotLinked
                | PullRequestError::Deleted
                | PullRequestError::Rejected(PullRequestRejection::LinkNotActive),
            ) => return,
            Err(error) => {
                tracing::warn!(%error, thread = %target.thread_id, "a pull request transition was not numbered");
                return;
            }
        };
        let mut claimed = Vec::new();
        for kind in notifications {
            let (storage, thread) = (self.storage.clone(), target.thread_id.clone());
            let notification = NotificationKey {
                link_revision: target.link_revision,
                transition_seq: seq,
                kind: kind.clone(),
            };
            if let Ok(Ok(true)) = self
                .blocking(move || storage.claim_pull_request_notification(&thread, notification))
                .await
            {
                claimed.push(kind);
            }
        }
        let pull_request = Self::reference(key);
        if let Some(status) = status {
            self.emit(PullRequestEvent::FetchStatusChanged {
                thread_id: target.thread_id.clone(),
                link_revision: target.link_revision,
                transition_seq: seq,
                pull_request: pull_request.clone(),
                status,
            });
        }
        if !fields.is_empty() {
            self.emit(PullRequestEvent::Changed {
                thread_id: target.thread_id.clone(),
                link_revision: target.link_revision,
                transition_seq: seq,
                pull_request,
                fields,
                notifications: claimed,
            });
        }
    }

    /// Ends a flight: records when the pull request is due next, removes the flight, tells the
    /// waiters, and wakes the scheduler.
    fn finish(&self, key: &PullRequestKey, flight: &Arc<Flight>, outcome: FlightOutcome) {
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
            let entry = state.keys.entry(key.clone()).or_insert(KeyState {
                next_due: Some(now),
                failures: 0,
            });
            let pending = Duration::from_secs(config.pending_poll_seconds);
            let stable = Duration::from_secs(config.stable_poll_seconds);
            let backoff_max = Duration::from_secs(config.backoff_max_seconds);
            match &outcome {
                FlightOutcome::Committed {
                    pending: is_pending,
                    terminal,
                    ..
                } => {
                    entry.failures = 0;
                    entry.next_due = if *terminal {
                        None
                    } else {
                        Some(now + if *is_pending { pending } else { stable })
                    };
                }
                FlightOutcome::Failed { error, .. } => {
                    entry.failures = entry.failures.saturating_add(1);
                    entry.next_due =
                        Some(now + backoff(error, pending, backoff_max, entry.failures));
                }
                FlightOutcome::StorageFailed(_) => {
                    entry.failures = entry.failures.saturating_add(1);
                    entry.next_due = Some(now + backoff_max.min(pending * 2));
                }
                FlightOutcome::Discarded => {
                    entry.next_due = Some(now);
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
                permit,
            }],
        );
        match flight.wait().await {
            FlightOutcome::Committed { seq, .. } => {
                let (storage, thread, at_ns) =
                    (self.storage.clone(), child.clone(), self.clock.now_ns());
                let resolution = self
                    .blocking(move || storage.resolve_pull_request_inheritance(&thread, seq, at_ns))
                    .await??;
                if let InheritanceResolution::Applied { link, .. } = &resolution {
                    let seq = self.next_transition(&child, link.link_revision).await?;
                    self.emit(PullRequestEvent::Linked {
                        thread_id: child.clone(),
                        link_revision: link.link_revision,
                        transition_seq: seq,
                        pull_request: Self::reference(&link.key),
                        origin: event_bus::PullRequestLinkOrigin::Inherited,
                    });
                    self.force(&child);
                }
                Ok(resolution)
            }
            FlightOutcome::Failed { error, .. } => {
                let (storage, thread, at_ns) = (self.storage.clone(), child, self.clock.now_ns());
                let note = error.failure().message;
                let _ = self
                    .blocking(move || {
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
