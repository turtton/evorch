//! Live cancellation precedes durable cancellation. Registry contention never
//! holds admission locks; only a current generation may eventually block a lane.
use super::*;
use crate::ownership::{OwnerPermit, RetryPolicy};
use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

const INITIAL_RETRY: Duration = Duration::from_millis(100);
const MAX_RETRY: Duration = Duration::from_secs(5);

struct FlushFailure {
    error: crate::RuntimeError,
    retry_storage: bool,
}

struct PendingStop {
    lane: String,
    ownership: Option<OwnerPermit>,
    reason: String,
    at_ns: i64,
}

#[derive(Default)]
pub(super) struct LaneStopWrites {
    pending: Mutex<HashMap<RunId, Arc<PendingStop>>>,
    changed: tokio::sync::Notify,
    drain: tokio::sync::Mutex<()>,
}

impl AgentRuntime {
    pub(crate) fn persist_lane_start_cancellation(
        &self,
        run: RunId,
        ownership: Option<&OwnerPermit>,
        reason: &str,
    ) -> Result<(), LaneError> {
        let Some(lane) = self
            .shared
            .lanes
            .creation_intents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&run)
            .cloned()
        else {
            return Ok(());
        };
        let writes = &self.shared.lanes.stops;
        let mut pending = writes.pending.lock().unwrap();
        let ownership = ownership.cloned().or_else(|| {
            pending
                .get(&run)
                .and_then(|previous| previous.ownership.clone())
        });
        let request = Arc::new(PendingStop {
            lane,
            ownership,
            reason: reason.into(),
            at_ns: self.lane_activity_time(),
        });
        if let Some(previous) = pending.get(&run) {
            // Cancel may harden a resumable Stop, never loosen it. Keep an
            // in-flight request immutable so its completion cannot erase this.
            if previous.reason == storage::lanes::LANE_START_OPERATOR_STOP
                && reason != storage::lanes::LANE_START_OPERATOR_STOP
            {
                pending.insert(run, request);
            }
            writes.changed.notify_one();
            return Err(LaneError::new(
                "lane_stop_pending",
                "durable cancellation queued",
            ));
        }
        pending.insert(run, request.clone());
        let attempt = (|| {
            let _guard = if let Some(permit) = &request.ownership {
                Some(
                    permit
                        .try_mutation_guard()
                        .map_err(|error| LaneError::new("lane_stop_write_failed", error))?
                        .ok_or_else(|| {
                            LaneError::new("lane_stop_pending", "ownership registry is busy")
                        })?,
                )
            } else {
                None
            };
            self.write_lane_stop(&request)
        })();
        if attempt.is_ok() {
            pending.remove(&run);
        } else {
            writes.changed.notify_one();
        }
        attempt
    }

    fn write_lane_stop(&self, request: &PendingStop) -> Result<(), LaneError> {
        let store = self.shared.run_store.get().ok_or_else(|| {
            LaneError::new("storage_unavailable", "lane ledger is not configured")
        })?;
        store
            .handle
            .block_lane_start(&request.lane, &request.reason, request.at_ns)?;
        Ok(())
    }

    #[must_use]
    pub fn pending_lane_stop_writes(&self) -> usize {
        self.shared.lanes.stops.pending.lock().unwrap().len()
    }

    async fn flush_lane_stop_writes(
        &self,
        policy: RetryPolicy,
        interrupted: Arc<AtomicBool>,
    ) -> Result<(), FlushFailure> {
        let writes = &self.shared.lanes.stops;
        let _drain = writes.drain.lock().await;
        loop {
            let pending = writes
                .pending
                .lock()
                .unwrap()
                .iter()
                .map(|(run, request)| (*run, request.clone()))
                .collect::<Vec<_>>();
            if pending.is_empty() {
                return Ok(());
            }
            let mut failure = None;
            let mut retry_storage = false;
            for (run, request) in pending {
                let result = async {
                    let _guard = if let Some(permit) = &request.ownership {
                        let interrupted = interrupted.clone();
                        Some(
                            permit
                                .mutation_guard_off_thread(policy, move || {
                                    interrupted.load(Ordering::Acquire)
                                })
                                .await
                                .map_err(|error| LaneError::new("lane_stop_write_failed", error))?,
                        )
                    } else {
                        None
                    };
                    #[cfg(test)]
                    self.lane_stop_write_hook(run, crate::runtime::HookPoint::LaneStopWriteStarted);
                    self.write_lane_stop(&request)?;
                    let mut pending = writes.pending.lock().unwrap();
                    if pending
                        .get(&run)
                        .is_some_and(|current| Arc::ptr_eq(current, &request))
                    {
                        pending.remove(&run);
                    }
                    drop(pending);
                    #[cfg(test)]
                    self.lane_stop_write_hook(run, crate::runtime::HookPoint::LaneStopPersisted);
                    tracing::info!(%run, lane = %request.lane, "lane cancellation is durable");
                    Ok::<_, LaneError>(())
                }
                .await;
                if let Err(error) = result {
                    retry_storage |= error.code == "storage_unavailable";
                    #[cfg(test)]
                    self.lane_stop_write_hook(run, crate::runtime::HookPoint::LaneStopWriteFailed);
                    let error = crate::RuntimeError::LaneCancellation {
                        run_id: run.to_string(),
                        reason: format!(
                            "live execution is stopped; durable cancellation was not recorded: {error}"
                        ),
                    };
                    tracing::error!(%run, %error, "lane cancellation writeback failed");
                    failure.get_or_insert(error);
                }
            }
            if let Some(error) = failure {
                return Err(FlushFailure {
                    error,
                    retry_storage,
                });
            }
        }
    }

    /// After requesting stop on the owning service, flush remaining writeback
    /// with the default ownership budget per queued lane. Failure retains the
    /// queue and is not equivalent to a durable operator stop.
    pub async fn drain_lane_stop_writes(&self) -> Result<(), crate::RuntimeError> {
        self.flush_lane_stop_writes(RetryPolicy::default(), Arc::new(AtomicBool::new(false)))
            .await
            .map_err(|failure| failure.error)
    }

    /// The service owns each retry through completion, including its blocking
    /// registry task. Stop interrupts that retry before the bounded final drain.
    pub async fn run_lane_stop_writes(
        &self,
        mut stop: tokio::sync::oneshot::Receiver<()>,
    ) -> Result<(), crate::RuntimeError> {
        let mut retry_storage = false;
        let mut backoff = INITIAL_RETRY;
        let mut timer = tokio::time::interval(INITIAL_RETRY);
        loop {
            if retry_storage {
                timer.reset_after(backoff);
                backoff = (backoff * 2).min(MAX_RETRY);
                tokio::select! {
                    biased;
                    _ = &mut stop => break,
                    _ = self.shared.lanes.stops.changed.notified() => {},
                    _ = timer.tick() => {}
                }
            } else {
                backoff = INITIAL_RETRY;
                tokio::select! {
                    biased;
                    _ = &mut stop => break,
                    _ = self.shared.lanes.stops.changed.notified() => {}
                }
            }
            let interrupted = Arc::new(AtomicBool::new(false));
            let work = self.flush_lane_stop_writes(RetryPolicy::unbounded(), interrupted.clone());
            tokio::pin!(work);
            let result = tokio::select! {
                biased;
                _ = &mut stop => {
                    interrupted.store(true, Ordering::Release);
                    let _ = work.await;
                    break;
                }
                result = &mut work => result,
            };
            retry_storage = result.is_err_and(|failure| failure.retry_storage);
        }
        self.drain_lane_stop_writes().await
    }
}
