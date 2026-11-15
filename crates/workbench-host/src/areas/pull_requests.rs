//! Host service of the `pull_requests` area (PR 15). Placeholder: the namespace is reserved,
//! so nothing reaches this service and it starts no task, opens no file and
//! talks to nothing. The PR that implements the area fills in this file, the
//! adapter `ingress/namespaces/pull_requests.rs` and the protocol module, and nothing
//! shared.
//!
//! Takes from the handles: `runtime` and its pull request services. It builds no source and no log source of its own.
use super::AreaHandles;
use crate::bootstrap::BootstrapError;

/// The `pull_requests` service, reached from `ingress/namespaces/pull_requests.rs` as
/// `self.areas.pull_requests`.
#[derive(Default)]
pub struct PullRequestsArea {}

impl PullRequestsArea {
    pub(super) fn new(_handles: &AreaHandles) -> Result<Self, BootstrapError> {
        Ok(Self::default())
    }
}
