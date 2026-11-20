//! Test-only: put a FAKE observation port behind the production observation
//! service. Bootstrap builds the service from the port in the area handles; the
//! authority (stored membership, run phase, Stop request), the notifications
//! through the projection and the lifecycle task are all production code, and
//! only the port is the test's. It is a fake injection: it proves nothing about
//! the `AgentObservationSource` that bootstrap installs in production.
//!
//! The port travels in a task-local of the future that opens the host, so no
//! signature of the bootstrap functions changes and a test that does not use
//! this entry sees nothing.
use super::{BootstrapError, BootstrapOptions, BootstrappedHost, TestSeams};
use crate::areas::browser::observation::AgentObservationPort;
use std::sync::Arc;

tokio::task_local! {
    static INJECTED: Arc<dyn AgentObservationPort>;
}

/// The port a test injected into the bootstrap that is running, if any.
pub(super) fn injected() -> Option<Arc<dyn AgentObservationPort>> {
    INJECTED.try_with(Clone::clone).ok()
}

impl BootstrappedHost {
    /// [`Self::open_with_test_seams`] with `port` as the agent observation
    /// port: the production service, authority and notifications over a fake.
    ///
    /// # Errors
    /// As [`Self::open_with_test_seams`].
    pub async fn open_with_agent_observation_port_for_test(
        options: BootstrapOptions,
        seams: TestSeams,
        port: Arc<dyn AgentObservationPort>,
    ) -> Result<Self, BootstrapError> {
        INJECTED
            .scope(port, Self::open_with_test_seams(options, seams))
            .await
    }
}
