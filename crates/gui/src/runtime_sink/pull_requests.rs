//! Pull request operations of the runtime command sink (ADR 0043 Phase 1).
//!
//! Link, Unlink, Refresh and Confirm act for a thread, so they need the permit of the process
//! that holds the thread's lease; the application obtains write mode first, as chat does.
//! They run on the Tokio runtime (storage and `gh` are never touched on the UI thread) and
//! report a refusal through [`CommandSink::poll`]. A success is not reported here: the
//! service publishes an event after it committed, and the display reads the database again.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};

use runtime::ownership::OwnerPermit;
use runtime::pull_requests::{DerivedThreadKind, PullRequestError, PullRequestService};

use crate::model::commands::{LoopEvent, PullRequestDerivation, WorkbenchCommand};

/// Refusals waiting for the next poll, and how many operations are still running.
pub(super) struct Jobs {
    sender: Sender<LoopEvent>,
    results: Receiver<LoopEvent>,
    running: Arc<AtomicUsize>,
    /// Refusals sent and not yet polled.
    unpolled: Arc<AtomicUsize>,
}

impl Default for Jobs {
    fn default() -> Self {
        let (sender, results) = channel();
        Self {
            sender,
            results,
            running: Arc::new(AtomicUsize::new(0)),
            unpolled: Arc::new(AtomicUsize::new(0)),
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
}

struct Running(Arc<AtomicUsize>);

impl Drop for Running {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
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
        if let WorkbenchCommand::InheritPullRequest {
            parent_thread_id,
            child_thread_id,
            kind,
        } = command
        {
            self.inherit_pull_request(&parent_thread_id, &child_thread_id, kind);
            return Vec::new();
        }
        let thread_id = match &command {
            WorkbenchCommand::LinkPullRequest { thread_id, .. }
            | WorkbenchCommand::UnlinkPullRequest { thread_id, .. }
            | WorkbenchCommand::RefreshPullRequest { thread_id }
            | WorkbenchCommand::ConfirmPullRequestCandidate { thread_id, .. }
            | WorkbenchCommand::DismissPullRequestCandidate { thread_id, .. } => thread_id.clone(),
            _ => return Vec::new(),
        };
        let refuse = |text: &str| {
            vec![LoopEvent::PullRequestRejected {
                thread_id: thread_id.clone(),
                reason: text.to_owned(),
            }]
        };
        let Some(service) = self.runtime.pull_requests() else {
            return refuse("Pull request tracking is not running");
        };
        let needs_permit = !matches!(
            command,
            WorkbenchCommand::DismissPullRequestCandidate { .. }
        );
        let permit = match permit {
            Some(permit) if permit.thread_id == thread_id => Some(permit),
            Some(_) => return refuse("The write-mode permit belongs to another thread"),
            None if needs_permit => {
                return refuse("Another window holds this thread in write mode");
            }
            None => None,
        };
        self.jobs_running().fetch_add(1, Ordering::SeqCst);
        let guard = Running(Arc::clone(self.jobs_running()));
        let sender = self.pull_request_jobs.sender.clone();
        let unpolled = Arc::clone(&self.pull_request_jobs.unpolled);
        self.handle.spawn(async move {
            let _running = guard;
            if let Err(error) = run(&service, command, permit).await {
                unpolled.fetch_add(1, Ordering::SeqCst);
                let _ = sender.send(LoopEvent::PullRequestRejected {
                    thread_id,
                    reason: reason(&error),
                });
            }
        });
        Vec::new()
    }

    fn jobs_running(&self) -> &Arc<AtomicUsize> {
        &self.pull_request_jobs.running
    }

    /// Pins the parent's link for a derived thread in the path that creates it, so what the
    /// parent does next (an Unlink) cannot take it away. A derived thread keeps no link of
    /// its own until the pull request is confirmed open.
    fn inherit_pull_request(&self, parent: &str, child: &str, kind: PullRequestDerivation) {
        let Some(service) = self.runtime.pull_requests() else {
            return;
        };
        let kind = match kind {
            PullRequestDerivation::Fork => DerivedThreadKind::Fork,
            PullRequestDerivation::Rewind => DerivedThreadKind::Rewind,
        };
        let _guard = self.handle.enter();
        if let Err(error) = service.inherit_now(parent, child, kind) {
            tracing::warn!(%error, parent, child, "pull request inheritance was not pinned");
        }
    }
}

async fn run(
    service: &PullRequestService,
    command: WorkbenchCommand,
    permit: Option<OwnerPermit>,
) -> Result<(), PullRequestError> {
    match (command, permit) {
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
            _,
        ) => service
            .dismiss_candidate(&thread_id, candidate_id)
            .await
            .map(drop),
        _ => Err(PullRequestError::NotOwner),
    }
}
