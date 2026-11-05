//! Pull request operations of the runtime command sink (ADR 0043 Phase 1).
//!
//! Link, Unlink, Refresh, Confirm and Dismiss act for a thread, so each needs the permit of the
//! process that holds the thread's lease (the application obtains write mode first, as chat
//! does, and this sink checks it again). An inheritance announces a derived thread and needs
//! none.
//!
//! Nothing here touches storage or `gh` on the UI thread. Every operation goes to one queue
//! that a single Tokio task works off in the order it was issued, so what a person did first
//! happens first: a fork's inheritance pins the parent's link before the Unlink the person
//! presses right after it, and an Unlink is never overtaken by a later Link. A refusal is
//! reported through [`CommandSink::poll`]; a success is not reported here, since the service
//! publishes an event after it committed and the display reads the database again.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};

use runtime::ownership::OwnerPermit;
use runtime::pull_requests::{DerivedThreadKind, PullRequestError, PullRequestService};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

use crate::model::commands::{LoopEvent, PullRequestDerivation, WorkbenchCommand};

/// One queued operation.
struct Work {
    service: PullRequestService,
    command: WorkbenchCommand,
    permit: Option<OwnerPermit>,
}

/// The queue, the refusals waiting for the next poll, and how much is still outstanding.
pub(super) struct Jobs {
    sender: Sender<LoopEvent>,
    results: Receiver<LoopEvent>,
    /// Operations queued or running.
    running: Arc<AtomicUsize>,
    /// Refusals sent and not yet polled.
    unpolled: Arc<AtomicUsize>,
    /// The queue of the worker task, started with the first operation.
    queue: Option<UnboundedSender<Work>>,
}

impl Default for Jobs {
    fn default() -> Self {
        let (sender, results) = channel();
        Self {
            sender,
            results,
            running: Arc::new(AtomicUsize::new(0)),
            unpolled: Arc::new(AtomicUsize::new(0)),
            queue: None,
        }
    }
}

impl Jobs {
    pub(super) fn poll(&self) -> Vec<LoopEvent> {
        let events: Vec<LoopEvent> = self.results.try_iter().collect();
        self.unpolled.fetch_sub(events.len(), Ordering::SeqCst);
        events
    }

    pub(super) fn busy(&self) -> bool {
        self.running.load(Ordering::SeqCst) > 0 || self.unpolled.load(Ordering::SeqCst) > 0
    }

    /// Queues `work`, starting the worker on `handle` the first time.
    fn enqueue(&mut self, handle: &tokio::runtime::Handle, work: Work) {
        let queue = self.queue.get_or_insert_with(|| {
            let (queue, mut inbox) = unbounded_channel::<Work>();
            let sender = self.sender.clone();
            let running = Arc::clone(&self.running);
            let unpolled = Arc::clone(&self.unpolled);
            handle.spawn(async move {
                while let Some(work) = inbox.recv().await {
                    let thread = work.command.thread().map(str::to_owned);
                    if let Err(error) = run(work).await
                        && let Some(thread_id) = thread
                    {
                        unpolled.fetch_add(1, Ordering::SeqCst);
                        let _ = sender.send(LoopEvent::PullRequestRejected {
                            thread_id,
                            reason: reason(&error),
                        });
                    }
                    running.fetch_sub(1, Ordering::SeqCst);
                }
            });
            queue
        });
        self.running.fetch_add(1, Ordering::SeqCst);
        if queue.send(work).is_err() {
            self.running.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

impl WorkbenchCommand {
    /// The thread a pull request operation acts for; `None` for any other command and for an
    /// inheritance (which names two threads and acts for neither).
    fn thread(&self) -> Option<&str> {
        match self {
            Self::LinkPullRequest { thread_id, .. }
            | Self::UnlinkPullRequest { thread_id, .. }
            | Self::RefreshPullRequest { thread_id }
            | Self::ConfirmPullRequestCandidate { thread_id, .. }
            | Self::DismissPullRequestCandidate { thread_id, .. } => Some(thread_id),
            _ => None,
        }
    }
}

/// The refusal as the person reads it.
fn reason(error: &PullRequestError) -> String {
    match error {
        PullRequestError::Disabled => "Pull request tracking is turned off".into(),
        PullRequestError::NotOwner | PullRequestError::OwnershipLost => {
            "Another window holds this thread in write mode".into()
        }
        PullRequestError::StaleRevision => {
            "The pull request link changed meanwhile; check it and try again".into()
        }
        other => other.to_string(),
    }
}

impl super::RuntimeCommandSink {
    pub(super) fn pull_requests_on(&self) -> bool {
        self.runtime
            .pull_requests()
            .is_some_and(|service| service.settings().enabled)
    }

    pub(super) fn pull_request_command(
        &mut self,
        command: WorkbenchCommand,
        permit: Option<OwnerPermit>,
    ) -> Vec<LoopEvent> {
        let Some(service) = self.runtime.pull_requests() else {
            // Without the service there is nothing to announce to and nothing to operate.
            return match command.thread() {
                Some(thread) => vec![LoopEvent::PullRequestRejected {
                    thread_id: thread.to_owned(),
                    reason: "Pull request tracking is not running".into(),
                }],
                None => Vec::new(),
            };
        };
        let permit = match command.thread() {
            None => None,
            Some(thread) => match permit {
                Some(permit) if permit.thread_id == thread => Some(permit),
                Some(_) => {
                    return vec![LoopEvent::PullRequestRejected {
                        thread_id: thread.to_owned(),
                        reason: "The write-mode permit belongs to another thread".into(),
                    }];
                }
                None => {
                    return vec![LoopEvent::PullRequestRejected {
                        thread_id: thread.to_owned(),
                        reason: "Another window holds this thread in write mode".into(),
                    }];
                }
            },
        };
        self.pull_request_jobs.enqueue(
            &self.handle,
            Work {
                service,
                command,
                permit,
            },
        );
        Vec::new()
    }
}

/// Runs one queued operation. The service checks the permit of the operations that take one;
/// dismissing a candidate takes none there, so its permit is checked here first.
async fn run(work: Work) -> Result<(), PullRequestError> {
    let Work {
        service,
        command,
        permit,
    } = work;
    match (command, permit) {
        (
            WorkbenchCommand::InheritPullRequest {
                parent_thread_id,
                child_thread_id,
                kind,
            },
            _,
        ) => {
            let kind = match kind {
                PullRequestDerivation::Fork => DerivedThreadKind::Fork,
                PullRequestDerivation::Rewind => DerivedThreadKind::Rewind,
            };
            if let Err(error) = service
                .inherit(&parent_thread_id, &child_thread_id, kind)
                .await
            {
                tracing::warn!(%error, parent = %parent_thread_id, child = %child_thread_id, "pull request inheritance was not pinned");
            }
            Ok(())
        }
        (
            WorkbenchCommand::LinkPullRequest {
                thread_id,
                locator,
                expected_revision,
            },
            Some(permit),
        ) => service
            .link(&thread_id, &locator, expected_revision, &permit)
            .await
            .map(drop),
        (
            WorkbenchCommand::UnlinkPullRequest {
                thread_id,
                expected_revision,
            },
            Some(permit),
        ) => service
            .unlink(&thread_id, expected_revision, &permit)
            .await
            .map(drop),
        (WorkbenchCommand::RefreshPullRequest { thread_id }, Some(permit)) => {
            service.refresh(&thread_id, &permit).await
        }
        (
            WorkbenchCommand::ConfirmPullRequestCandidate {
                thread_id,
                candidate_id,
                expected_revision,
            },
            Some(permit),
        ) => service
            .confirm_candidate(&thread_id, candidate_id, expected_revision, &permit)
            .await
            .map(drop),
        (
            WorkbenchCommand::DismissPullRequestCandidate {
                thread_id,
                candidate_id,
            },
            Some(permit),
        ) => {
            let held = tokio::task::spawn_blocking(move || permit.validate_generation().is_ok())
                .await
                .unwrap_or(false);
            if !held {
                return Err(PullRequestError::NotOwner);
            }
            service
                .dismiss_candidate(&thread_id, candidate_id)
                .await
                .map(drop)
        }
        _ => Err(PullRequestError::NotOwner),
    }
}
