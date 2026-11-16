//! Planned exit deliberately does not use operator shutdown, release owners, or
//! stop the SQLite writer. The coordinator retains this object even on failure.
use super::*;
use std::time::Duration;
use tokio::task::JoinHandle;

// D-2b design values, unchanged. D-2c-3 measures the real composition.
const REAP_LIMIT: Duration = Duration::from_secs(1);
const JOIN_LIMIT: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ExitStage {
    SpawnFenceClosed = 1,
    StopRequested = 2,
    ChildrenReaped = 3,
    WorkersJoined = 4,
    StorageDrained = 5,
}
struct Workers {
    services: services::BackgroundServices,
    reclaim: Option<runtime::thread_retention::ReclaimExecutor>,
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
}
impl BootstrappedHost {
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
    ) -> Result<PlannedExit, String> {
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
        Ok(PlannedExit {
            ledger: self.spawn_ledger.clone(),
            runtime: self.runtime.clone(),
            area_tasks: self.area_tasks.clone(),
            workers: Some(Workers {
                services: std::mem::take(&mut self.services),
                reclaim: self.reclaim.take(),
                tasks,
            }),
            joining: None,
            fence: None,
            bridge: Some(bridge),
            flushing: None,
            writer,
            completed: None,
            failure: None,
        })
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
            match stage {
                ExitStage::SpawnFenceClosed => {
                    let ledger = self.ledger.clone();
                    let task = self.fence.get_or_insert_with(|| {
                        tokio::task::spawn_blocking(move || ledger.close_fence())
                    });
                    (&mut *task)
                        .await
                        .map_err(|_| "spawn fence worker panicked")?;
                    self.fence = None;
                }
                ExitStage::StopRequested => {
                    let workers = self.workers.as_mut().ok_or("planned workers missing")?;
                    workers.services.request_planned_stop();
                    self.area_tasks.request_stop();
                    if let Some(reclaim) = &workers.reclaim {
                        reclaim.request_stop();
                    }
                    // Stops the separately owned scheduler; finite cleanup was
                    // drained and synchronously sealed before capture.
                    self.runtime.learning_gate().close();
                    if let Some(pr) = self.runtime.pull_requests() {
                        pr.shutdown();
                    }
                }
                ExitStage::ChildrenReaped => {
                    self.ledger.stop_all_children();
                    tokio::time::timeout(REAP_LIMIT, self.ledger.reap_all_children())
                        .await
                        .map_err(|_| "owned child reap deadline exceeded")?
                        .map_err(|_| "owned child reap unconfirmed")?;
                }
                ExitStage::WorkersJoined => {
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
                                failure
                                    .get_or_insert_with(|| "owned ledger worker panicked".into());
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
                            if let Some(reclaim) = workers.reclaim.take() {
                                remember(
                                    &mut failure,
                                    tokio::task::spawn_blocking(move || reclaim.try_shutdown())
                                        .await
                                        .map_err(|_| "reclaim join worker panicked".into())
                                        .and_then(|r| {
                                            r.map_err(|_| "reclaim worker panicked".into())
                                        }),
                                );
                            }
                            (workers, failure.map_or(Ok(()), Err))
                        }));
                    }
                    let (workers, result) =
                        tokio::time::timeout(JOIN_LIMIT, self.joining.as_mut().unwrap())
                            .await
                            .map_err(|_| "owned worker join deadline exceeded")?
                            .map_err(|_| "planned join worker panicked")?;
                    self.joining = None;
                    self.workers = Some(workers);
                    result?;
                }
                ExitStage::StorageDrained => {
                    if self.flushing.is_none() {
                        let mut bridge = self.bridge.take().ok_or("planned bridge missing")?;
                        self.flushing = Some(tokio::task::spawn_blocking(move || {
                            // ACK of the last business event + usage, then finite
                            // subscriber stop/join; the writer itself stays alive.
                            let result = bridge
                                .flush()
                                .map_err(|e| e.to_string())
                                .and_then(|()| bridge.try_shutdown().map_err(|e| e.to_string()))
                                .and_then(|()| {
                                    bridge.handle().barrier().map_err(|e| e.to_string())
                                });
                            (bridge, result)
                        }));
                    }
                    // No new bridge deadline: D-2c-3 must measure it first.
                    let (bridge, result) = self
                        .flushing
                        .as_mut()
                        .unwrap()
                        .await
                        .map_err(|_| "planned bridge worker panicked")?;
                    self.flushing = None;
                    self.bridge = Some(bridge);
                    result?;
                }
            }
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
