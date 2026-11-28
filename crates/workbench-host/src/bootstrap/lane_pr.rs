//! Core-side startup, durable lane recovery and producer drain.
use super::{BootstrapError, services::BackgroundServices};
use runtime::pull_requests::source::TokioProcessRunner;
use runtime::{AgentRuntime, pull_requests::*};
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

impl PullRequestAdapters {
    /// Production core readers share one runner and the process-local spawn fence.
    pub fn owned(ledger: sandbox::SpawnLedger) -> Self {
        let runner = Arc::new(TokioProcessRunner::with_ledger(ledger));
        Self {
            source: Arc::new(GhSource::with_runner(runner.clone())),
            logs: Arc::new(GhLogSource::with_runner(runner)),
        }
    }
}

/// Synchronous first recovery precedes command/transport admission.
pub fn start_recovery(
    runtime: &AgentRuntime,
    scratch: &Arc<runtime::scratch::ThreadScratch>,
) -> Result<tokio::task::JoinHandle<()>, runtime::lanes::LaneError> {
    runtime.set_thread_scratch(scratch.clone());
    runtime.recover_lanes_on_startup()
}

/// Call inside the host executor, after storage and tombstone fences are
/// installed, and before publishing the ready host. Retain services through drain,
/// including when a later startup step fails.
pub fn start(
    services: &mut BackgroundServices,
    runtime: &AgentRuntime,
    scratch: &Arc<runtime::scratch::ThreadScratch>,
    settings: config::PullRequestsConfig,
    adapters: PullRequestAdapters,
) -> Result<(), BootstrapError> {
    settings.validate()?;
    // Subscribe before recovery can admit a root on another executor thread.
    let service =
        runtime.start_pull_requests_with_log_source(adapters.source, settings, adapters.logs)?;
    services.start("pull-requests", |stop| async move {
        let _ = stop.await;
        service.shutdown_and_drain().await?;
        Ok(())
    });
    runtime.set_thread_scratch(scratch.clone());
    let recovery = runtime.prepare_lane_recovery()?;
    services.start("lane-recovery", |stop| async move {
        // Retry storage operations are synchronous; dropping this future leaves
        // no child task. Admitted roots are joined by the runtime drain.
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
    ) -> Result<(), BootstrapError> {
        stop_and_drain(services, &self.runtime, &self.executor).await
    }
}

/// The same teardown is used for successful startup and partial recovery failure.
/// Only lane producers join before the runtime snapshot; other services can await
/// runtime termination without forming a shutdown dependency cycle.
pub async fn stop_and_drain(
    services: &mut BackgroundServices,
    runtime: &AgentRuntime,
    executor: &tools::ToolExecutor,
) -> Result<(), BootstrapError> {
    let mut failure = None;
    // Reuse the established 03b prefix; no restart quiesce in operator drain.
    let fence = runtime
        .fence_dispatch_shutdown(|| stop_dispatch_producers(services, runtime))
        .await;
    if let Err(error) = &fence {
        remember(&mut failure, Err::<(), _>(error.clone()));
    }
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
    if let Ok(fence) = fence {
        remember(&mut failure, runtime.drain_dispatch_tails(&fence).await);
    }
    remember(&mut failure, services.shutdown().await);
    failure.map_or(Ok(()), Err)
}

/// The existing lane producer/admission/service stop order is the fence receipt.
async fn stop_dispatch_producers(
    services: &mut BackgroundServices,
    runtime: &AgentRuntime,
) -> Result<(), BootstrapError> {
    let mut failure = None;
    if let Some(service) = runtime.pull_requests() {
        service.shutdown();
    }
    // Only recovery producers may stop yet. Learning observers must remain
    // subscribed until no recovery/admission can register another root.
    remember(&mut failure, services.join_named("graph-recovery").await);
    remember(&mut failure, services.join_named("lane-recovery").await);
    remember(&mut failure, runtime.drain_lane_admissions().await);
    // A recovery scan already in progress may have acquired another lease.
    services.request_stop();
    failure.map_or(Ok(()), Err)
}
