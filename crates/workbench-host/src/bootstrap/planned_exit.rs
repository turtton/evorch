//! Planned exit retains thread runtime owners and the SQLite writer. The index
//! owner alone stops at StopRequested, before the storage barrier. The coordinator
//! retains this object even on failure.
use super::*;
mod holds;
pub use holds::RestartHolds;
use std::time::Duration;
use tokio::task::JoinHandle;

// D-2b design values, unchanged.
const REAP_LIMIT: Duration = Duration::from_secs(1);
const JOIN_LIMIT: Duration = Duration::from_secs(1);
// One budget for flush + subscriber/bridge-worker join + writer barrier.
// D-2c-3 observed a healthy local maximum flush ACK of 3.8054s; 10s allows
// headroom, not a fleet-tail guarantee. Expiry prohibits M2, never cancels I/O.
const STORAGE_DRAIN_LIMIT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ExitStage {
    SpawnFenceClosed = 1,
    StopRequested = 2,
    ChildrenReaped = 3,
    WorkersJoined = 4,
    StorageDrained = 5,
    TargetChecked = 6,
    OwnersHeld = 7,
    Armed = 8,
    ExecPrepared = 9,
    SignalsStopped = 10,
    OwnerStopped = 11,
}
struct Workers {
    services: services::BackgroundServices,
    reclaim: Option<runtime::thread_retention::ReclaimExecutor>,
    auto_archive: Option<crate::thread_retention::AutoArchiveOwner>,
    index_owner: Option<thread_index::ThreadIndexOwner>,
    tasks: Vec<JoinHandle<()>>,
}
/// Owned, non-serializable cleanup state. No method grants exec permission.
/// A timed-out join remains in this owner; no abort/detach/discard fallback.
pub struct PlannedExit {
    ledger: sandbox::SpawnLedger,
    runtime: AgentRuntime,
    area_tasks: AreaTasks,
    workers: Option<Workers>,
    joining: Option<JoinHandle<(Workers, Result<(), String>)>>,
    fence: Option<JoinHandle<()>>,
    bridge: Option<OwnedStorageBridge>,
    flushing: Option<JoinHandle<(OwnedStorageBridge, Result<(), String>)>>,
    writer: storage::StorageHandle,
    completed: Option<ExitStage>,
    failure: Option<String>,
    workers_result: Option<Result<(), String>>,
    cleanup_result: Option<Result<(), String>>,
    holds: Arc<RestartHolds>,
}
impl BootstrappedHost {
    /// The bridge moved here, so ordinary shutdown must reclaim it before
    /// releasing owners/scratch. As before, this cleanup wait has no deadline.
    pub(super) async fn cleanup_planned_exit(&mut self) -> Result<(), String> {
        if let Some(resources) = &self.planned_exit {
            tracing::debug!(target: "restart_drain", phase = "planned_shutdown_cleanup_begin");
            let result = resources.lock().await.cleanup().await;
            tracing::debug!(target: "restart_drain", phase = "planned_shutdown_cleanup_end");
            result
        } else {
            Ok(())
        }
    }

    /// Validate before the coordinator mutex: host Stop takes host -> coordinator,
    /// so capture must never take these locks in the reverse order.
    pub fn validate_planned_exit(&self, gate: &restart::HostRestartGate) -> Result<(), String> {
        let own = self.host.lock().unwrap().service().restart_gate();
        if !gate.same_instance(&own)
            || own.phase() != restart::HostRestartPhase::Sealed
            || self.bridge.is_none()
        {
            return Err("planned exit requires this host and an owned bridge".into());
        }
        Ok(())
    }
    /// Transfer AFTER the atomic capture mutex is released. A concurrent shutdown
    /// must not strand the irreversible attempt without its cleanup resources.
    pub fn take_planned_exit(
        &mut self,
        gate: &restart::HostRestartGate,
    ) -> Result<Arc<tokio::sync::Mutex<PlannedExit>>, String> {
        let command = self.host.lock().unwrap();
        let own = command.service().restart_gate();
        if !gate.same_instance(&own)
            || !matches!(
                own.phase(),
                restart::HostRestartPhase::Capturing
                    | restart::HostRestartPhase::FailedClosed
                    | restart::HostRestartPhase::ShuttingDown
            )
        {
            return Err("planned exit requires this host's sealed admission".into());
        }
        drop(command);
        if let Some(mut history) = self.history_owner.take() {
            history.stop();
        }
        let bridge = self
            .bridge
            .take()
            .ok_or("planned storage bridge is not owned")?;
        let writer = bridge.handle();
        let tasks = self
            .host
            .lock()
            .unwrap()
            .service_mut()
            .take_background_tasks();
        let resources = Arc::new(tokio::sync::Mutex::new(PlannedExit {
            ledger: self.spawn_ledger.clone(),
            runtime: self.runtime.clone(),
            area_tasks: self.area_tasks.clone(),
            workers: Some(Workers {
                services: std::mem::take(&mut self.services),
                reclaim: self.reclaim.take(),
                auto_archive: self.auto_archive.take(),
                index_owner: self.thread_index_owner.take(),
                tasks,
            }),
            joining: None,
            fence: None,
            bridge: Some(bridge),
            flushing: None,
            writer: writer.clone(),
            completed: None,
            failure: None,
            workers_result: None,
            cleanup_result: None,
            holds: Arc::new(RestartHolds::new(
                self.owner.clone(),
                self.restart_storage.clone(),
                writer,
            )),
        }));
        self.planned_exit = Some(resources.clone());
        Ok(resources)
    }
}
impl PlannedExit {
    pub fn completed(&self) -> Option<ExitStage> {
        self.completed
    }
    pub fn writer(&self) -> storage::StorageHandle {
        self.writer.clone()
    }
    /// Run M2 1..5 in order. The callback checks the coordinator shutdown/exec
    /// prohibition latch both before and after each stage. It is not a receipt
    /// authorizing stage 6 or exec; the caller MUST remain failed-closed there.
    pub async fn first_five(
        &mut self,
        mut checkpoint: impl FnMut(Option<ExitStage>) -> Result<(), String>,
    ) -> Result<(), String> {
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        let result = self.run(&mut checkpoint).await;
        if let Err(error) = &result {
            self.failure = Some(error.clone());
        }
        result
    }
    /// Independent resource reclamation, never an M2 progress/exec receipt.
    /// No business resume. A timed-out M2 join is awaited to actual completion.
    /// The host and coordinator share this owner, including its sticky result.
    pub async fn cleanup(&mut self) -> Result<(), String> {
        // Retry uncertain durable invalidation/hold removal even when ordinary
        // resource cleanup has a sticky result. Never reopen business work.
        let holds = self.holds.clone();
        let holds_result = tokio::task::spawn_blocking(move || holds.cleanup())
            .await
            .map_err(|_| "hold cleanup worker panicked".to_owned())
            .and_then(|r| r);
        if let Some(result) = &self.cleanup_result {
            return result.clone().and(holds_result);
        }
        self.failure
            .get_or_insert_with(|| "resource cleanup has begun; M2 prohibited".into());
        let mut failure = holds_result.err();
        // Stop producers first even when capture failed before stage 1.
        remember(
            &mut failure,
            self.run_stage(ExitStage::StopRequested, false).await,
        );
        for stage in [
            ExitStage::SpawnFenceClosed,
            ExitStage::ChildrenReaped,
            ExitStage::WorkersJoined,
            ExitStage::StorageDrained,
        ] {
            if self.completed.is_some_and(|done| done >= stage) {
                continue;
            }
            remember(&mut failure, self.run_stage(stage, false).await);
        }
        let result = failure.map_or(Ok(()), Err);
        self.cleanup_result = Some(result.clone());
        result
    }
    pub fn restart_holds(&self) -> Arc<RestartHolds> {
        self.holds.clone()
    }
    async fn run_stage(&mut self, stage: ExitStage, bounded: bool) -> Result<(), String> {
        match stage {
            ExitStage::TargetChecked
            | ExitStage::OwnersHeld
            | ExitStage::Armed
            | ExitStage::ExecPrepared
            | ExitStage::SignalsStopped
            | ExitStage::OwnerStopped => {
                return Err("later stages belong to the coordinator".into());
            }
            ExitStage::SpawnFenceClosed => {
                let ledger = self.ledger.clone();
                let task = self.fence.get_or_insert_with(|| {
                    tokio::task::spawn_blocking(move || ledger.close_fence())
                });
                let result = (&mut *task).await;
                self.fence = None;
                result.map_err(|_| "spawn fence worker panicked")?;
            }
            ExitStage::StopRequested => {
                if let Some(owner) = self.workers.as_mut().and_then(|w| w.index_owner.as_mut()) {
                    owner.stop().map_err(|e| e.to_string())?;
                }
                if let Some(workers) = self.workers.as_mut() {
                    workers.services.request_planned_stop();
                    if let Some(owner) = &workers.auto_archive {
                        owner.request_stop();
                    }
                    if let Some(reclaim) = &workers.reclaim {
                        reclaim.request_stop();
                    }
                }
                self.area_tasks.request_stop();
                // Stops the separately owned scheduler; finite cleanup was
                // drained and synchronously sealed before capture.
                self.runtime.learning_gate().close();
                if let Some(pr) = self.runtime.pull_requests() {
                    pr.shutdown();
                }
            }
            ExitStage::ChildrenReaped => {
                self.ledger.stop_all_children();
                if bounded {
                    tokio::time::timeout(REAP_LIMIT, self.ledger.reap_all_children())
                        .await
                        .map_err(|_| "owned child reap deadline exceeded")?
                        .map_err(|_| "owned child reap unconfirmed")?;
                } else {
                    self.ledger
                        .reap_all_children()
                        .await
                        .map_err(|_| "owned child reap unconfirmed")?;
                }
            }
            ExitStage::WorkersJoined => {
                if let Some(result) = &self.workers_result {
                    return result.clone();
                }
                if self.joining.is_none() {
                    let mut workers = self.workers.take().ok_or("planned workers missing")?;
                    let area = self.area_tasks.clone();
                    let runtime = self.runtime.clone();
                    let ledger = self.ledger.clone();
                    self.joining = Some(tokio::spawn(async move {
                        let mut failure = None;
                        remember(
                            &mut failure,
                            ledger.join_workers().await.map_err(|e| e.to_string()),
                        );
                        if ledger.worker_stats().panicked != 0 {
                            failure.get_or_insert_with(|| "owned ledger worker panicked".into());
                        }
                        remember(
                            &mut failure,
                            workers.services.shutdown().await.map_err(|e| e.to_string()),
                        );
                        for task in workers.tasks.drain(..) {
                            remember(
                                &mut failure,
                                task.await
                                    .map_err(|_| "host background task panicked".into()),
                            );
                        }
                        remember(
                            &mut failure,
                            area.shutdown().await.map_err(|e| e.to_string()),
                        );
                        remember(&mut failure, runtime.join_learning_for_planned_exit().await);
                        if let Some(mut owner) = workers.auto_archive.take() {
                            remember(
                                &mut failure,
                                tokio::task::spawn_blocking(move || owner.shutdown())
                                    .await
                                    .map_err(|_| "auto archive join panicked".to_owned())
                                    .and_then(|r| r),
                            );
                        }
                        if let Some(reclaim) = workers.reclaim.take() {
                            remember(
                                &mut failure,
                                tokio::task::spawn_blocking(move || reclaim.try_shutdown())
                                    .await
                                    .map_err(|_| "reclaim join worker panicked".into())
                                    .and_then(|r| r.map_err(|_| "reclaim worker panicked".into())),
                            );
                        }
                        (workers, failure.map_or(Ok(()), Err))
                    }));
                }
                let joined = if bounded {
                    tokio::time::timeout(JOIN_LIMIT, self.joining.as_mut().unwrap())
                        .await
                        .map_err(|_| "owned worker join deadline exceeded")?
                } else {
                    self.joining.as_mut().unwrap().await
                };
                self.joining = None;
                let result = match joined {
                    Ok((workers, result)) => {
                        self.workers = Some(workers);
                        result
                    }
                    Err(_) => Err("planned join worker panicked".into()),
                };
                self.workers_result = Some(result.clone());
                result?;
            }
            ExitStage::StorageDrained => {
                if self.flushing.is_none() {
                    let mut bridge = self.bridge.take().ok_or("planned bridge missing")?;
                    self.flushing = Some(tokio::task::spawn_blocking(move || {
                        let mut failure = None;
                        remember(&mut failure, bridge.flush().map_err(|e| e.to_string()));
                        // The same task may outlive its M2 budget. Always reclaim
                        // the subscriber, including a late failed/unknown ACK;
                        // cleanup must not inherit an unjoined short-circuit.
                        remember(
                            &mut failure,
                            bridge.try_shutdown().map_err(|e| e.to_string()),
                        );
                        if !bounded || failure.is_none() {
                            tracing::debug!(target: "restart_drain", phase = "writer_barrier_begin");
                            let result = bridge.handle().barrier().map_err(|e| e.to_string());
                            tracing::debug!(target: "restart_drain", phase = "writer_barrier_end", ok = result.is_ok());
                            remember(&mut failure, result);
                        }
                        let result = failure.map_or(Ok(()), Err);
                        (bridge, result)
                    }));
                }
                let joined = if bounded {
                    // Borrow the handle: timeout drops only this wait, not the
                    // task/bridge. FailedClosed cleanup awaits this exact task
                    // without a new budget; late success cannot advance M2.
                    tokio::time::timeout(STORAGE_DRAIN_LIMIT, self.flushing.as_mut().unwrap())
                        .await
                        .map_err(|_| "storage drain deadline exceeded")?
                } else {
                    self.flushing.as_mut().unwrap().await
                };
                self.flushing = None;
                let (bridge, result) = joined.map_err(|_| "planned bridge worker panicked")?;
                self.bridge = Some(bridge);
                result?;
            }
        }
        Ok(())
    }
    async fn run(
        &mut self,
        checkpoint: &mut impl FnMut(Option<ExitStage>) -> Result<(), String>,
    ) -> Result<(), String> {
        for stage in [
            ExitStage::SpawnFenceClosed,
            ExitStage::StopRequested,
            ExitStage::ChildrenReaped,
            ExitStage::WorkersJoined,
            ExitStage::StorageDrained,
        ] {
            if self.completed.is_some_and(|done| done >= stage) {
                continue;
            }
            checkpoint(self.completed)?;
            self.run_stage(stage, true).await?;
            self.completed = Some(stage);
            checkpoint(self.completed)?;
        }
        Ok(())
    }
}
fn remember(failure: &mut Option<String>, result: Result<(), String>) {
    if let Err(error) = result {
        failure.get_or_insert(error);
    }
}
