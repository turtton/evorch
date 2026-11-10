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
    let writer = runtime.clone();
    services.start("lane-stop-writes", |stop| async move {
        writer.run_lane_stop_writes(stop).await?;
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

pub(crate) fn remember(
    failure: &mut Option<BootstrapError>,
    result: Result<(), impl Into<BootstrapError>>,
) {
    if let Err(error) = result {
        failure.get_or_insert_with(|| error.into());
    }
}

/// Retain runtime shutdown resources without exposing tool types to GUI adapters.
pub struct RuntimeDrain {
    runtime: AgentRuntime,
    executor: Arc<tools::ToolExecutor>,
}

impl RuntimeDrain {
    pub fn new(runtime: AgentRuntime, executor: Arc<tools::ToolExecutor>) -> Self {
        Self { runtime, executor }
    }

    pub async fn stop_and_drain(
        &self,
        services: &mut BackgroundServices,
        owner: &Arc<OwnerHost>,
    ) -> Result<(), BootstrapError> {
        stop_and_drain(services, &self.runtime, owner, &self.executor).await
    }
}

/// The same teardown is used for successful startup and partial recovery failure.
/// Only lane producers join before the runtime snapshot; other services can await
/// runtime termination without forming a shutdown dependency cycle.
pub async fn stop_and_drain(
    services: &mut BackgroundServices,
    runtime: &AgentRuntime,
    owner: &OwnerHost,
    executor: &tools::ToolExecutor,
) -> Result<(), BootstrapError> {
    let mut failure = None;
    remember(&mut failure, owner.begin_quiesce().map(|_| ()));
    if let Some(service) = runtime.pull_requests() {
        service.shutdown();
    }
    services.request_stop();
    remember(&mut failure, services.join_named("lane-recovery").await);
    remember(&mut failure, runtime.drain_lane_admissions().await);
    // A recovery scan already in progress may have acquired another lease.
    remember(&mut failure, owner.begin_quiesce().map(|_| ()));
    let runs = runtime.list_agents();
    for run in &runs {
        if matches!(
            run.phase,
            event_bus::AgentRunPhase::Pending
                | event_bus::AgentRunPhase::Running
                | event_bus::AgentRunPhase::Waiting
        ) {
            remember(
                &mut failure,
                runtime.stop(run.run_id, runtime::StopScope::Subtree),
            );
        }
    }
    for run in runs {
        remember(&mut failure, runtime.wait(run.run_id).await.map(|_| ()));
        executor.cancel_shell_jobs(&run.run_id.to_string());
        remember(
            &mut failure,
            executor.drain_shell_jobs(&run.run_id.to_string()).await,
        );
    }
    remember(&mut failure, runtime.drain_lane_admissions().await);
    remember(&mut failure, runtime.drain_lane_stop_writes().await);
    remember(&mut failure, services.shutdown().await);
    failure.map_or(Ok(()), Err)
}
