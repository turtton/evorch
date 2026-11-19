//! A narrow read-only execution reader, not durable resume evidence.
use super::*;
#[derive(Clone)]
pub struct BrowserExecutionReader {
    runtime: WeakAgentRuntime,
}
pub struct BrowserExecutionObservation {
    pub incarnation: String,
    current: Arc<dyn Fn() -> bool + Send + Sync>,
    interrupt: watch::Receiver<RunInterrupt>,
}
impl BrowserExecutionObservation {
    /// Waits for invalidation of the registered execution incarnation only.
    ///
    /// This is not a run-completion or Stop notification: Done and Stopped
    /// leave [`Self::current`] true while the same incarnation stays registered.
    /// O-H must determine run termination/Stop and authorize live observation;
    /// `current() == true` is never permission to observe.
    pub async fn invalidated(&mut self) {
        while self.current() {
            if self.interrupt.changed().await.is_err() {
                return;
            }
        }
    }
    /// Checks only whether the registered execution incarnation is still current.
    ///
    /// Done and Stopped do not by themselves make this false; it remains true
    /// while that incarnation stays valid and registered. O-H owns run
    /// termination/Stop checks and live-observation authorization. Callers must
    /// not interpret `true` as permission to observe.
    pub fn current(&self) -> bool {
        (self.current)()
    }
}
impl BrowserExecutionReader {
    /// Current stored affiliation, not caller-supplied scope or resume evidence.
    pub fn affiliation(&self, run: RunId) -> Option<(String, String)> {
        let runtime = self.runtime.upgrade()?;
        let thread = runtime
            .with_storage_reader(|db| {
                let Some(link) = db.thread_of_run(&run.to_string())? else {
                    return Ok(None);
                };
                db.thread(&link.thread_id)
            })
            .ok()
            .flatten()
            .flatten()?;
        Some((thread.project_id?, thread.thread_id))
    }
    pub fn observe(&self, run: RunId) -> Option<BrowserExecutionObservation> {
        let runtime = self.runtime.upgrade()?;
        let execution = runtime.execution_snapshot(run).ok()?;
        if execution.cancelled() {
            return None;
        }
        let incarnation = execution.publication().incarnation();
        let weak = self.runtime.clone();
        let observed = Arc::downgrade(&execution);
        Some(BrowserExecutionObservation {
            incarnation,
            interrupt: execution.cancel_tx.subscribe(),
            current: Arc::new(move || {
                let Some(runtime) = weak.upgrade() else {
                    return false;
                };
                runtime.execution_snapshot(run).is_ok_and(|now| {
                    !now.cancelled()
                        && observed
                            .upgrade()
                            .is_some_and(|old| Arc::ptr_eq(&old, &now))
                })
            }),
        })
    }
}
impl AgentRuntime {
    pub fn browser_execution_reader(&self) -> BrowserExecutionReader {
        BrowserExecutionReader {
            runtime: self.downgrade(),
        }
    }
}
