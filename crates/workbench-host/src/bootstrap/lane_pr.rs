//! Shared startup for core and the transitional embedded GUI.
//!
//! Authority is per thread (ADR 0024), not per window or process-wide DB access:
//! recovery claims the lane thread before starting it; PR producers obtain and
//! revalidate the same owner's permit before observing, fetching or preparing a
//! notice. Another host cannot act for that thread merely by opening this DB.
//! Transitional only: P11 (PR20) removes ownership and replaces this partition
//! with a single core-side service owner; do not keep it as a second core path.
use super::{BootstrapError, services::BackgroundServices};
use runtime::{AgentRuntime, ownership::OwnerHost, pull_requests::*};
use std::sync::Arc;

pub struct PullRequestAdapters {
    pub source: Arc<dyn PullRequestSource>,
    pub logs: Arc<dyn PullRequestLogSource>,
}
impl Default for PullRequestAdapters {
    fn default() -> Self {
        Self {
            source: Arc::new(GhSource::new()),
            logs: Arc::new(GhLogSource::new()),
        }
    }
}

/// Synchronous first recovery precedes command/transport admission. Retries only
/// cover unexpired leases left by an external host; they never steal live owners.
pub fn start_recovery(
    runtime: &AgentRuntime,
    owner: &Arc<OwnerHost>,
) -> Result<tokio::task::JoinHandle<()>, runtime::lanes::LaneError> {
    runtime.set_thread_scratch(owner.thread_scratch());
    runtime.recover_lanes_on_startup(owner)
}

/// Call inside the host executor, after storage and tombstone fences are
/// installed, and before publishing the ready host. Retain services through drain,
/// including when a later startup step fails.
pub fn start(
    services: &mut BackgroundServices,
    runtime: &AgentRuntime,
    owner: &Arc<OwnerHost>,
    settings: config::PullRequestsConfig,
    adapters: PullRequestAdapters,
) -> Result<(), BootstrapError> {
    settings.validate()?;
    // Subscribe before recovery can admit a root on another executor thread.
    let view: Arc<dyn OwnershipView> = owner.clone();
    let service = runtime.start_pull_requests_with_log_source(
        view,
        adapters.source,
        settings,
        adapters.logs,
    )?;
    services.start("pull-requests", |stop| async move {
        let _ = stop.await;
        service.shutdown_and_drain().await?;
        Ok(())
    });
    runtime.set_thread_scratch(owner.thread_scratch());
    let recovery = runtime.prepare_lane_recovery(owner)?;
    services.start("lane-recovery", |stop| async move {
        // Retry storage operations are synchronous; dropping this future leaves
        // no child task. Admitted roots are drained by the runtime owner host.
        tokio::select! { biased; _ = stop => {}, _ = recovery => {} }
        Ok(())
    });
    Ok(())
}
