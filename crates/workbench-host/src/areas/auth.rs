//! Host service of the `auth` area (PR 10). Placeholder: the namespace is reserved,
//! so nothing reaches this service and it starts no task, opens no file and
//! talks to nothing. The PR that implements the area fills in this file, the
//! adapter `ingress/namespaces/auth.rs` and the protocol module, and nothing
//! shared.
//!
//! Takes from the handles: `credentials` (the store the runs read) and `tasks` for a login that waits for a person (see `workbench_protocol::operation`: such a wait holds no restart permit).
use super::AreaHandles;
use crate::bootstrap::BootstrapError;

/// The `auth` service, reached from `ingress/namespaces/auth.rs` as
/// `self.areas.auth`.
#[derive(Default)]
pub struct AuthArea {}

impl AuthArea {
    pub(super) fn new(_handles: &AreaHandles) -> Result<Self, BootstrapError> {
        Ok(Self::default())
    }
}
