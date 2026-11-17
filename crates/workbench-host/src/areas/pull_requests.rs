//! Host service of the `pull_requests` area (PR 15): the pull request a thread
//! tracks, as every client sees it, and the operations that change it.
//!
//! The pull request service of the runtime stays the only writer. This area
//! never runs `gh`, never opens the database for writing and never decides
//! whether an automatic notice may be sent: it translates the protocol into the
//! service's calls, keeps the table of operations (so a repeated request is
//! answered from it instead of running twice, and a client that lost the answer
//! reads the outcome later) and turns what the service published on the bus
//! into events of the namespace.
//!
//! Handlers never wait. A command becomes an operation that runs as a counted
//! area task and keeps the restart permit it was accepted with until its outcome
//! is settled. The link, the unlink and every policy change quote revisions
//! that the service compares inside its writer, so two clients that read the
//! same revision end as one success and one `stale_revision`.
//!
//! A failure after the effect may have been written (storage) settles as
//! `outcome_unknown`; an operation whose task was cut off (a panic, the stop of
//! the host) settles the same way and is never reported as a success.
//!
//! The one notification of an automatic fix stop at its limit is claimed here,
//! through the storage writer's compare-and-set, before the event is published:
//! several clients, and several cores on one database, tell the person once.
//!
//! Takes from the handles: `runtime` and its pull request service (looked up
//! when used, since the service starts after the areas), `storage` for the
//! claim, `bus`, `tasks` and the `pull_requests` event source.
use super::settings::operations::{Lookup, Operations, Tracked, digest_of};
use super::{AreaHandles, ProjectionSource};
use crate::bootstrap::BootstrapError;
use crate::bootstrap::restart::HostCommandPermit;
use event_bus::{EventKind, PullRequestChangedField, PullRequestEvent, RecvError};
use runtime::AgentRuntime;
use runtime::ownership::OwnerPermit;
use runtime::pull_requests::{PullRequestError, PullRequestService};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use storage::pull_request_auto_fix::{PR_AUTO_FIX_LIMIT, PrAutoFixRejection};
use storage::pull_requests as stored;
use workbench_protocol::{
    errors::{ErrorCode, RetryAdvice, WorkbenchError},
    events::UiEvent,
    ids::{OperationId, ThreadId},
    namespaces::pull_requests::{
        AUTO_FIX_LIMIT, AutoFixView, CandidateState, CandidateView, CheckView, FactsView,
        FetchFailureView, HeadAttribution, LinkOrigin, LinkView, MAX_MESSAGE_BYTES, MAX_OPERATIONS,
        NoticeMode, NoticePhase, NoticeView, PullRequestChange, PullRequestOperationView,
        PullRequestRef, PullRequestState, PullRequestsCommand, PullRequestsEvent, ReviewDecision,
        SnapshotView, ThreadPullRequest,
    },
    operation::{OperationState, OperationView},
    wire::{Revision, Sequence, UnixMillis},
};

impl Tracked for PullRequestOperationView {
    fn operation(&self) -> &OperationView {
        &self.operation
    }

    fn operation_mut(&mut self) -> &mut OperationView {
        &mut self.operation
    }
}

/// The `pull_requests` service, reached from `ingress/namespaces/pull_requests.rs`
/// as `self.areas.pull_requests`. Without handles (the unwired form) every
/// operation is refused.
#[derive(Default, Clone)]
pub struct PullRequestsArea {
    inner: Option<Arc<Inner>>,
}

struct Inner {
    runtime: AgentRuntime,
    tasks: super::AreaTasks,
    source: ProjectionSource,
    operations: Mutex<Operations<PullRequestOperationView>>,
    /// Taken after `operations` when both are held.
    seen: Mutex<Seen>,
}

/// How many accepted requests the area still recognises after their operation
/// left the table (which keeps [`MAX_OPERATIONS`] for reading outcomes).
const MAX_SEEN: usize = 8192;

/// The id and request digest of every accepted command, kept far longer than
/// the operation itself. A request sent again after its operation was evicted
/// is answered as an unknown outcome and never runs a second time, and the
/// same id with another request is still a conflict. The bound is only on how
/// many later commands may be accepted before a resend is no longer recognised.
#[derive(Default)]
struct Seen {
    digests: HashMap<OperationId, [u8; 32]>,
    order: VecDeque<OperationId>,
}

impl Seen {
    fn lookup(&self, id: &OperationId, digest: &[u8; 32]) -> Lookup<()> {
        match self.digests.get(id) {
            None => Lookup::New,
            Some(known) if known == digest => Lookup::Same(()),
            Some(_) => Lookup::Conflict,
        }
    }

    fn remember(&mut self, id: &OperationId, digest: [u8; 32]) {
        if self.digests.insert(id.clone(), digest).is_none() {
            self.order.push_back(id.clone());
        }
        while self.order.len() > MAX_SEEN {
            if let Some(oldest) = self.order.pop_front() {
                self.digests.remove(&oldest);
            }
        }
    }

    fn forget(&mut self, id: &OperationId) {
        if self.digests.remove(id).is_some() {
            self.order.retain(|known| known != id);
        }
    }
}

/// How an accepted command stands.
pub struct Submitted {
    pub view: PullRequestOperationView,
    /// The same operation id and request were accepted before.
    pub already_applied: bool,
}

/// What a finished operation leaves for the client to quote next.
#[derive(Default)]
struct Done {
    lifecycle_revision: Option<u64>,
    policy_revision: Option<u64>,
    notice_id: Option<String>,
}

struct Failure {
    code: ErrorCode,
    message: String,
    /// The effect may have been written.
    unknown: bool,
}

fn error(code: ErrorCode, message: &str, retry: RetryAdvice) -> WorkbenchError {
    WorkbenchError::new(code, message, retry)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A bounded sentence without anything that looks like a credential.
fn sanitize(text: &str) -> String {
    let redacted = secret_guard::SecretRedactor::from_env().redact(text).text;
    let mut end = redacted.len().min(MAX_MESSAGE_BYTES);
    while !redacted.is_char_boundary(end) {
        end -= 1;
    }
    redacted[..end].replace(char::is_control, " ")
}

fn failure(code: ErrorCode, message: &str) -> Failure {
    Failure {
        code,
        message: message.to_owned(),
        unknown: false,
    }
}

impl From<PullRequestError> for Failure {
    fn from(error: PullRequestError) -> Self {
        use PullRequestError as E;
        match error {
            E::ProducerStopped => failure(ErrorCode::Draining, "the core is stopping"),
            E::Disabled => failure(
                ErrorCode::UnsupportedCapability,
                "pull request tracking is disabled",
            ),
            E::Ineligible => failure(
                ErrorCode::Conflict,
                "the pull request head or checks are not eligible",
            ),
            E::Locator(error) => Self {
                code: ErrorCode::InvalidRequest,
                message: sanitize(&error.to_string()),
                unknown: false,
            },
            E::NotOwner | E::OwnershipLost => failure(
                ErrorCode::PermissionDenied,
                "this core does not hold the thread",
            ),
            E::StaleRevision => failure(
                ErrorCode::StaleRevision,
                "the thread's pull request changed since it was read; read it again",
            ),
            E::NotLinked => failure(ErrorCode::NotFound, "the thread has no pull request link"),
            E::CandidateNotFound => failure(ErrorCode::NotFound, "unknown pull request candidate"),
            E::Deleted => failure(ErrorCode::NotFound, "the thread was deleted"),
            E::Rejected(rejection) => Self {
                code: ErrorCode::Conflict,
                message: sanitize(&rejection.to_string()),
                unknown: false,
            },
            E::OwnershipBusy => failure(ErrorCode::Busy, "the ownership registry is busy"),
            E::Storage(storage::StorageError::PrAutoFix(rejection)) => match rejection {
                PrAutoFixRejection::StaleLink | PrAutoFixRejection::StaleBudget => failure(
                    ErrorCode::StaleRevision,
                    "the automatic fix policy changed since it was read; read it again",
                ),
                PrAutoFixRejection::NotLinked => {
                    failure(ErrorCode::NotFound, "the thread has no pull request link")
                }
                other => Self {
                    code: ErrorCode::Conflict,
                    message: sanitize(&other.to_string()),
                    unknown: false,
                },
            },
            E::Storage(_) => Self {
                code: ErrorCode::StorageUnavailable,
                message: "the pull request state could not be stored; read it again".into(),
                unknown: true,
            },
            E::Fetch(_) => failure(ErrorCode::Internal, "the pull request could not be fetched"),
        }
    }
}

impl PullRequestsArea {
    pub(super) fn new(handles: &AreaHandles) -> Result<Self, BootstrapError> {
        let source = handles.source("pull_requests")?;
        let inner = Arc::new(Inner {
            runtime: handles.runtime.clone(),
            tasks: handles.tasks.clone(),
            source,
            operations: Mutex::new(Operations::new(MAX_OPERATIONS)),
            seen: Mutex::default(),
        });
        // Subscribed before the task starts, so a change during startup is announced.
        let mut events = handles.bus.subscribe();
        let watcher = inner.clone();
        handles
            .tasks
            .spawn("pull-requests-events", move |mut stop| async move {
                loop {
                    let event = tokio::select! {
                        () = stop.stopped() => break,
                        event = events.recv() => event,
                    };
                    match event {
                        Ok(event) => {
                            if let EventKind::PullRequest(event) = &event.kind {
                                watcher.announce(event).await;
                            }
                        }
                        Err(RecvError::Lagged(_)) => {}
                        Err(RecvError::Closed) => break,
                    }
                }
                Ok(())
            })
            .map_err(|error| BootstrapError::from(error.to_string()))?;
        Ok(Self { inner: Some(inner) })
    }

    fn inner(&self) -> Result<&Arc<Inner>, WorkbenchError> {
        self.inner.as_ref().ok_or_else(|| {
            error(
                ErrorCode::UnsupportedCapability,
                "pull requests are not available in this host",
                RetryAdvice::Never,
            )
        })
    }

    /// Accept a command: start its operation, or answer from the table.
    /// `permit` is asked only for a new operation, so a repeated request is
    /// answered even when the thread has meanwhile left this core.
    ///
    /// # Errors
    /// `Conflict` for an id used by a different request, `LimitExceeded` when
    /// every retained operation is still running, `Draining` once the host is
    /// stopping, and what `permit` refuses with.
    pub(crate) fn submit(
        &self,
        command: PullRequestsCommand,
        accepted: Option<HostCommandPermit>,
        permit: impl FnOnce() -> Result<OwnerPermit, WorkbenchError>,
    ) -> Result<Submitted, WorkbenchError> {
        let inner = self.inner()?;
        let id = command.operation_id().clone();
        let digest = digest_of(&[serde_json::to_vec(&command).unwrap_or_default().as_slice()]);
        if let Some(known) = inner.known(&command, &id, &digest)? {
            return Ok(known);
        }
        let permit = permit()?;
        let view = {
            let mut operations = lock(&inner.operations);
            if let Some(known) = inner.answer(&mut operations, &command, &id, &digest)? {
                return Ok(known);
            }
            let revision = operations.next_revision();
            let view = PullRequestOperationView {
                operation: OperationView {
                    operation_id: id.clone(),
                    state: OperationState::Pending,
                    revision,
                    result: None,
                },
                kind: command.kind(),
                thread: command.thread().clone(),
                lifecycle_revision: None,
                policy_revision: None,
                notice_id: None,
                message: None,
                outcome_unknown: false,
            };
            operations.insert(view.clone(), digest)?;
            lock(&inner.seen).remember(&id, digest);
            view
        };
        let task = inner.clone();
        let task_id = id.clone();
        let spawned =
            inner
                .tasks
                .spawn_counted_tracked("pull-request-operation", accepted, move |_stop| {
                    task.run(task_id, command, permit)
                });
        if let Err(refused) = spawned {
            lock(&inner.operations).forget(&id);
            lock(&inner.seen).forget(&id);
            return Err(error(
                ErrorCode::Draining,
                &refused.to_string(),
                RetryAdvice::AfterReconnect,
            ));
        }
        Ok(Submitted {
            view,
            already_applied: false,
        })
    }

    /// What a client shows of `thread`, read from the database the service
    /// writes. `owned_here` is what the adapter knows of the thread's lease.
    ///
    /// # Errors
    /// `StorageUnavailable` when the database cannot be read.
    pub fn thread(
        &self,
        thread: &ThreadId,
        owned_here: bool,
    ) -> Result<ThreadPullRequest, WorkbenchError> {
        let inner = self.inner()?;
        let service = inner.runtime.pull_requests();
        let settings = service.as_ref().map(PullRequestService::settings);
        let read = inner
            .runtime
            .with_storage_reader(|db| read_thread(db, thread.as_str()))
            .map_err(storage_error)?
            .ok_or_else(|| {
                error(
                    ErrorCode::StorageUnavailable,
                    "the event store is not open",
                    RetryAdvice::AfterReconnect,
                )
            })?;
        let attribution = match (&service, &read.link) {
            (Some(service), Some(_)) => match service.head_attribution(thread.as_str()) {
                runtime::pull_requests::HeadAttribution::Agent => HeadAttribution::Agent,
                runtime::pull_requests::HeadAttribution::Unknown => HeadAttribution::Unknown,
            },
            _ => HeadAttribution::Unknown,
        };
        let globally = settings.as_ref().is_some_and(|s| s.auto_fix_enabled);
        Ok(read.into_view(
            thread.clone(),
            settings.as_ref().is_some_and(|s| s.enabled),
            owned_here,
            globally,
            attribution,
        ))
    }

    /// One fix notice.
    ///
    /// # Errors
    /// `NotFound` for an id the core does not hold.
    pub fn notice(&self, notice_id: &str) -> Result<NoticeView, WorkbenchError> {
        let inner = self.inner()?;
        let notice = inner
            .runtime
            .with_storage_reader(|db| db.pr_notice(notice_id))
            .map_err(storage_error)?
            .flatten()
            .ok_or_else(|| error(ErrorCode::NotFound, "unknown notice", RetryAdvice::Never))?;
        notice_view(&notice)
            .ok_or_else(|| error(ErrorCode::Internal, "invalid notice", RetryAdvice::Never))
    }

    /// One operation, running or finished.
    ///
    /// # Errors
    /// `NotFound` for an id the core does not hold.
    pub fn operation(&self, id: &OperationId) -> Result<PullRequestOperationView, WorkbenchError> {
        lock(&self.inner()?.operations)
            .get(id)
            .cloned()
            .ok_or_else(|| error(ErrorCode::NotFound, "unknown operation", RetryAdvice::Never))
    }

    /// Wait until `id` is settled and return it.
    ///
    /// # Errors
    /// `NotFound` for an id the core does not hold.
    pub async fn wait_operation(
        &self,
        id: &OperationId,
    ) -> Result<PullRequestOperationView, WorkbenchError> {
        let mut changes = lock(&self.inner()?.operations).subscribe();
        loop {
            let view = self.operation(id)?;
            if view.operation.state.is_final() {
                return Ok(view);
            }
            if changes.changed().await.is_err() {
                return Ok(view);
            }
        }
    }

    /// The retained operations, newest first.
    ///
    /// # Errors
    /// Only the unwired form refuses.
    pub fn operations(&self) -> Result<Vec<PullRequestOperationView>, WorkbenchError> {
        Ok(lock(&self.inner()?.operations).list())
    }
}

fn conflict() -> WorkbenchError {
    error(
        ErrorCode::Conflict,
        "this operation id was used for a different request",
        RetryAdvice::Never,
    )
}

fn storage_error(_: storage::StorageError) -> WorkbenchError {
    error(
        ErrorCode::StorageUnavailable,
        "the pull request state could not be read",
        RetryAdvice::AfterReconnect,
    )
}

impl Inner {
    fn known(
        &self,
        command: &PullRequestsCommand,
        id: &OperationId,
        digest: &[u8; 32],
    ) -> Result<Option<Submitted>, WorkbenchError> {
        let mut operations = lock(&self.operations);
        self.answer(&mut operations, command, id, digest)
    }

    /// What the area already says of a request: the retained operation, the
    /// unknown outcome of one that was evicted, a conflict, or nothing.
    fn answer(
        &self,
        operations: &mut Operations<PullRequestOperationView>,
        command: &PullRequestsCommand,
        id: &OperationId,
        digest: &[u8; 32],
    ) -> Result<Option<Submitted>, WorkbenchError> {
        match operations.lookup(id, digest) {
            Lookup::Same(view) => {
                return Ok(Some(Submitted {
                    view,
                    already_applied: true,
                }));
            }
            Lookup::Conflict => return Err(conflict()),
            Lookup::New => {}
        }
        match lock(&self.seen).lookup(id, digest) {
            Lookup::New => Ok(None),
            Lookup::Conflict => Err(conflict()),
            Lookup::Same(()) => {
                // Accepted before and its record is gone: whether it took
                // effect is unknown, and it must not run again.
                let revision = operations.next_revision();
                Ok(Some(Submitted {
                    view: PullRequestOperationView {
                        operation: OperationView {
                            operation_id: id.clone(),
                            state: OperationState::Failed {
                                code: ErrorCode::Internal,
                            },
                            revision,
                            result: None,
                        },
                        kind: command.kind(),
                        thread: command.thread().clone(),
                        lifecycle_revision: None,
                        policy_revision: None,
                        notice_id: None,
                        message: Some(
                            "this request was accepted earlier and its outcome is no longer kept; read the thread's pull request again"
                                .into(),
                        ),
                        outcome_unknown: true,
                    },
                    already_applied: true,
                }))
            }
        }
    }

    fn publish(&self, event: PullRequestsEvent) {
        if let Err(error) = self.source.publish(UiEvent::PullRequests(event)) {
            tracing::warn!(code = ?error.code, "pull request event could not be published");
        }
    }

    async fn run(
        self: Arc<Self>,
        id: OperationId,
        command: PullRequestsCommand,
        permit: OwnerPermit,
    ) -> Result<(), BootstrapError> {
        // Settles the operation as unknown when this future is dropped or
        // panics before it finished: a cut-off operation is never a success.
        let mut guard = Settle {
            inner: self.clone(),
            id: id.clone(),
            settled: false,
        };
        let outcome = self.execute(command, &permit).await;
        guard.settled = true;
        self.finish(&id, outcome);
        Ok(())
    }

    async fn execute(
        &self,
        command: PullRequestsCommand,
        permit: &OwnerPermit,
    ) -> Result<Done, Failure> {
        let service = self.runtime.pull_requests().ok_or_else(|| {
            failure(
                ErrorCode::UnsupportedCapability,
                "pull request tracking is not running",
            )
        })?;
        let done = match command {
            PullRequestsCommand::LinkPullRequest(c) => {
                let receipt = service
                    .link(
                        c.thread.as_str(),
                        &c.locator,
                        c.expected_revision.get(),
                        permit,
                    )
                    .await?;
                lifecycle(receipt.lifecycle_revision)
            }
            PullRequestsCommand::UnlinkPullRequest(c) => {
                let receipt = service
                    .unlink(c.thread.as_str(), c.expected_revision.get(), permit)
                    .await?;
                lifecycle(receipt.lifecycle_revision)
            }
            PullRequestsCommand::RefreshPullRequest(c) => {
                service.refresh(c.thread.as_str(), permit).await?;
                Done::default()
            }
            PullRequestsCommand::ConfirmPullRequestCandidate(c) => {
                let receipt = service
                    .confirm_candidate(
                        c.thread.as_str(),
                        c.candidate_id,
                        c.expected_revision.get(),
                        permit,
                    )
                    .await?;
                lifecycle(receipt.lifecycle_revision)
            }
            PullRequestsCommand::DismissPullRequestCandidate(c) => {
                service
                    .dismiss_candidate(c.thread.as_str(), c.candidate_id, permit)
                    .await?;
                Done::default()
            }
            PullRequestsCommand::SetAutoFix(c) => {
                let state = service
                    .set_auto_fix(
                        c.thread.as_str(),
                        c.link_revision.get(),
                        c.policy_revision.get(),
                        c.enabled,
                        permit,
                    )
                    .await?;
                policy(state.policy_revision)
            }
            PullRequestsCommand::ResetAutoFixBudget(c) => {
                let state = service
                    .reset_auto_fix_budget(
                        c.thread.as_str(),
                        c.link_revision.get(),
                        c.policy_revision.get(),
                        c.confirmed_previous.map(String::from),
                        permit,
                    )
                    .await?;
                policy(state.policy_revision)
            }
            PullRequestsCommand::ChangeAutoFixDestination(c) => {
                let state = service
                    .change_auto_fix_destination(
                        c.thread.as_str(),
                        c.link_revision.get(),
                        c.policy_revision.get(),
                        c.confirmed_previous.map(String::from),
                        permit,
                    )
                    .await?;
                policy(state.policy_revision)
            }
            PullRequestsCommand::RequestManualNotice(c) => {
                let notice = service
                    .request_manual_notice(
                        c.thread.as_str(),
                        c.link_revision.get(),
                        &c.expected_head,
                        permit,
                    )
                    .await?;
                Done {
                    notice_id: Some(notice.notice_id),
                    ..Done::default()
                }
            }
        };
        Ok(done)
    }

    fn finish(&self, id: &OperationId, outcome: Result<Done, Failure>) {
        let view = lock(&self.operations).update(id, |view| match outcome {
            Ok(done) => {
                view.operation.state = OperationState::Ready;
                view.lifecycle_revision = done.lifecycle_revision.map(Revision);
                view.policy_revision = done.policy_revision.map(Revision);
                view.notice_id = done.notice_id;
            }
            Err(failure) => {
                view.operation.state = OperationState::Failed { code: failure.code };
                view.message = Some(failure.message);
                view.outcome_unknown = failure.unknown;
            }
        });
        if let Some(view) = view {
            self.publish(PullRequestsEvent::PullRequestOperationCompleted { operation: view });
        }
    }

    /// Translate what the service published, and take the one notification of
    /// a limit stop when the change may have reached it.
    async fn announce(self: &Arc<Self>, event: &PullRequestEvent) {
        let Some(change) = translate(event) else {
            return;
        };
        let thread = change_thread(event).to_owned();
        self.publish(change);
        if matches!(event, PullRequestEvent::AutoFixChanged { .. }) {
            let inner = self.clone();
            // One closure for the claim and the event: when the watcher is
            // stopped meanwhile the claim is not left without its event.
            let _ = tokio::task::spawn_blocking(move || inner.announce_limit(&thread)).await;
        }
    }

    /// Announce the stop of `thread`'s pull request at the limit when the
    /// service hands this caller its one notification.
    fn announce_limit(&self, thread: &str) {
        let Some(service) = self.runtime.pull_requests() else {
            return;
        };
        match service.take_limit_notification(thread) {
            Ok(Some(notice)) => {
                let Ok(thread) = ThreadId::new(thread) else {
                    return;
                };
                self.publish(PullRequestsEvent::AutoFixLimitReached {
                    thread,
                    pull_request: reference(&notice.key),
                    link_revision: Revision(notice.link_revision),
                    budget_epoch: notice.budget_epoch,
                    limit: AUTO_FIX_LIMIT,
                });
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(%error, "the limit notification could not be claimed");
            }
        }
    }
}

/// Settles an operation as unknown unless [`Settle::settled`] was set.
struct Settle {
    inner: Arc<Inner>,
    id: OperationId,
    settled: bool,
}

impl Drop for Settle {
    fn drop(&mut self) {
        if !self.settled {
            self.inner.finish(
                &self.id,
                Err(Failure {
                    code: ErrorCode::Internal,
                    message: "the operation was cut off; read the thread's pull request again"
                        .into(),
                    unknown: true,
                }),
            );
        }
    }
}

fn lifecycle(revision: u64) -> Done {
    Done {
        lifecycle_revision: Some(revision),
        ..Done::default()
    }
}

fn policy(revision: u64) -> Done {
    Done {
        policy_revision: Some(revision),
        ..Done::default()
    }
}

fn change_thread(event: &PullRequestEvent) -> &str {
    match event {
        PullRequestEvent::Linked { thread_id, .. }
        | PullRequestEvent::Unlinked { thread_id, .. }
        | PullRequestEvent::CandidateChanged { thread_id, .. }
        | PullRequestEvent::Changed { thread_id, .. }
        | PullRequestEvent::FetchStatusChanged { thread_id, .. }
        | PullRequestEvent::AutoFixChanged { thread_id, .. } => thread_id,
    }
}

/// The event of the namespace for a bus event; `None` for a thread id the
/// protocol cannot name.
fn translate(event: &PullRequestEvent) -> Option<PullRequestsEvent> {
    let (change, link_revision, transition_seq, pull_request, notifications) = match event {
        PullRequestEvent::Linked {
            link_revision,
            transition_seq,
            pull_request,
            ..
        } => (
            PullRequestChange::Linked,
            *link_revision,
            *transition_seq,
            pull_request,
            Vec::new(),
        ),
        PullRequestEvent::Unlinked {
            link_revision,
            transition_seq,
            pull_request,
            ..
        } => (
            PullRequestChange::Unlinked,
            *link_revision,
            *transition_seq,
            pull_request,
            Vec::new(),
        ),
        PullRequestEvent::CandidateChanged {
            link_revision,
            transition_seq,
            pull_request,
            ..
        } => (
            PullRequestChange::CandidateChanged,
            *link_revision,
            *transition_seq,
            pull_request,
            Vec::new(),
        ),
        PullRequestEvent::Changed {
            link_revision,
            transition_seq,
            pull_request,
            notifications,
            fields,
            ..
        } => {
            // A change of the checkout head alone is not a change of the facts.
            let _ = fields
                .iter()
                .all(|f| *f == PullRequestChangedField::LocalHead);
            (
                PullRequestChange::FactsChanged,
                *link_revision,
                *transition_seq,
                pull_request,
                notifications.clone(),
            )
        }
        PullRequestEvent::FetchStatusChanged {
            link_revision,
            transition_seq,
            pull_request,
            ..
        } => (
            PullRequestChange::FetchStatusChanged,
            *link_revision,
            *transition_seq,
            pull_request,
            Vec::new(),
        ),
        PullRequestEvent::AutoFixChanged {
            link_revision,
            transition_seq,
            pull_request,
            ..
        } => (
            PullRequestChange::AutoFixChanged,
            *link_revision,
            *transition_seq,
            pull_request,
            Vec::new(),
        ),
    };
    Some(PullRequestsEvent::PullRequestChanged {
        thread: ThreadId::new(change_thread(event)).ok()?,
        pull_request: PullRequestRef {
            provider: pull_request.provider.clone(),
            host: pull_request.host.clone(),
            repository: pull_request.repository.clone(),
            number: pull_request.number,
        },
        change,
        link_revision: Revision(link_revision),
        transition_seq: Sequence(transition_seq),
        notifications,
    })
}

fn reference(key: &stored::PullRequestKey) -> PullRequestRef {
    PullRequestRef {
        provider: key.provider.as_str().to_owned(),
        host: key.host.clone(),
        repository: key.repository.clone(),
        number: key.number,
    }
}

fn millis(ns: i64) -> UnixMillis {
    UnixMillis(u64::try_from(ns / 1_000_000).unwrap_or(0))
}

/// What one read of the database found for a thread.
struct ThreadRead {
    state: stored::ThreadPullRequestState,
    link: Option<stored::PullRequestLink>,
    snapshot: Option<stored::PullRequestSnapshot>,
    candidates: Vec<stored::PullRequestCandidate>,
    auto_fix: Option<storage::pull_request_auto_fix::PrAutoFixState>,
}

fn read_thread(db: &storage::Database, thread: &str) -> Result<ThreadRead, storage::StorageError> {
    let state = db.pull_request_thread_state(thread)?;
    let link = state.active_link.clone();
    let (snapshot, auto_fix) = match &link {
        Some(link) => (
            db.pull_request_snapshot(&link.key)?,
            Some(db.pr_auto_fix_state(&link.key)?),
        ),
        None => (None, None),
    };
    let candidates = db.pull_request_candidates(thread, None)?;
    Ok(ThreadRead {
        state,
        link,
        snapshot,
        candidates,
        auto_fix,
    })
}

impl ThreadRead {
    fn into_view(
        self,
        thread: ThreadId,
        tracking_enabled: bool,
        owned_here: bool,
        globally_enabled: bool,
        head_attribution: HeadAttribution,
    ) -> ThreadPullRequest {
        let local_head = self
            .state
            .local_head
            .as_ref()
            .map(|head| head.head_sha.clone());
        ThreadPullRequest {
            thread,
            tracking_enabled,
            owned_here,
            lifecycle_revision: Revision(self.state.lifecycle_revision),
            transition_seq: Sequence(self.state.transition_seq),
            link: self.link.as_ref().map(|link| LinkView {
                pull_request: reference(&link.key),
                link_revision: Revision(link.link_revision),
                origin: match link.origin {
                    stored::LinkOrigin::Explicit => LinkOrigin::Explicit,
                    stored::LinkOrigin::Inherited => LinkOrigin::Inherited,
                },
                linked_at: millis(link.created_at_ns),
            }),
            local_head,
            snapshot: self.snapshot.as_ref().map(snapshot_view),
            candidates: self
                .candidates
                .iter()
                .map(|candidate| CandidateView {
                    candidate_id: candidate.candidate_id,
                    pull_request: reference(&candidate.key),
                    state: match candidate.state {
                        stored::CandidateState::Presented => CandidateState::Presented,
                        stored::CandidateState::Dismissed => CandidateState::Dismissed,
                        stored::CandidateState::Confirmed => CandidateState::Confirmed,
                    },
                })
                .collect(),
            auto_fix: self.auto_fix.as_ref().map(|state| AutoFixView {
                enabled: state.enabled,
                globally_enabled,
                destination: state
                    .destination_thread_id
                    .as_deref()
                    .and_then(|thread| ThreadId::new(thread).ok()),
                link_revision: state.link_revision.map(Revision),
                policy_revision: Revision(state.policy_revision),
                used: state.used,
                reserved: state.reserved,
                limit: PR_AUTO_FIX_LIMIT,
                cooldown_until: state.cooldown_until_ns.map(millis),
                stop_reason: state.stop_reason.clone(),
                limit_reached: state.stop_reason.as_deref() == Some("limit_reached")
                    && state.limit_reached_epoch == Some(state.budget_epoch),
            }),
            head_attribution,
        }
    }
}

fn snapshot_view(snapshot: &stored::PullRequestSnapshot) -> SnapshotView {
    SnapshotView {
        facts: snapshot.facts.as_ref().map(|facts| FactsView {
            state: match facts.state {
                stored::PullRequestState::Open => PullRequestState::Open,
                stored::PullRequestState::Closed => PullRequestState::Closed,
                stored::PullRequestState::Merged => PullRequestState::Merged,
            },
            base_branch: facts.base_branch.clone(),
            head_repository: facts.head_repository.clone(),
            head_branch: facts.head_branch.clone(),
            head_sha: facts.head_sha.clone(),
            checks: facts
                .checks
                .iter()
                .map(|check| CheckView {
                    name: check.name.clone(),
                    kind: check.kind.as_str().to_owned(),
                    status: check.status.as_str().to_owned(),
                    conclusion: check.conclusion.map(|c| c.as_str().to_owned()),
                })
                .collect(),
            checks_complete: facts.checks_complete,
            review_decision: match facts.review_decision {
                stored::ReviewDecision::Approved => ReviewDecision::Approved,
                stored::ReviewDecision::ChangesRequested => ReviewDecision::ChangesRequested,
                stored::ReviewDecision::ReviewRequired => ReviewDecision::ReviewRequired,
                stored::ReviewDecision::None => ReviewDecision::None,
            },
        }),
        title: snapshot
            .display
            .title
            .as_ref()
            .map(|title| title.as_str().to_owned()),
        fetched_at: snapshot.fetched_at_ns.map(millis),
        last_failure: snapshot
            .last_attempt
            .as_ref()
            .and_then(|attempt| attempt.failure.as_ref())
            .map(|failure| FetchFailureView {
                kind: failure.kind.as_str().to_owned(),
                message: sanitize(&failure.message),
            }),
        consecutive_failures: u32::try_from(snapshot.consecutive_failures).unwrap_or(u32::MAX),
    }
}

fn notice_view(notice: &storage::pull_request_auto_fix::PrNotice) -> Option<NoticeView> {
    use storage::pull_request_auto_fix::{PrNoticeMode, PrNoticePhase};
    Some(NoticeView {
        notice_id: notice.notice_id.clone(),
        pull_request: reference(&notice.key),
        head_sha: notice.head_sha.clone(),
        mode: match notice.mode {
            PrNoticeMode::Manual => NoticeMode::Manual,
            PrNoticeMode::Auto => NoticeMode::Auto,
        },
        destination: notice
            .destination_thread_id
            .as_deref()
            .and_then(|thread| ThreadId::new(thread).ok()),
        link_revision: Revision(notice.link_revision),
        policy_revision: Revision(notice.policy_revision),
        phase: match notice.phase {
            PrNoticePhase::Reserved => NoticePhase::Reserved,
            PrNoticePhase::Ready => NoticePhase::Ready,
            PrNoticePhase::Adopted => NoticePhase::Adopted,
            PrNoticePhase::Cancelled => NoticePhase::Cancelled,
        },
        payload: notice.payload.clone(),
    })
}
