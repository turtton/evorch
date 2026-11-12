//! Explicit dispatch work tracking. Run lifetimes (including parked loops) are NOT tails.
use crate::{AgentRuntime, RuntimeError};
use std::{
    cell::RefCell,
    future::Future,
    sync::{Arc, Mutex},
};
use tokio::sync::watch;

#[derive(Default)]
struct State {
    count: usize,
    closed: bool,
    failure: Option<String>,
    drained: bool,
}
pub(crate) struct DispatchTails {
    state: Mutex<State>,
    version: watch::Sender<u64>,
}
impl Default for DispatchTails {
    fn default() -> Self {
        Self {
            state: Mutex::default(),
            version: watch::channel(0).0,
        }
    }
}
/// Clone keeps the same registration alive; fork registers a descendant BEFORE spawn.
#[derive(Clone)]
pub(crate) struct DispatchGuard(Arc<Lease>);
struct Lease {
    tails: Arc<DispatchTails>,
    unfinished: std::sync::atomic::AtomicBool,
}
impl Drop for Lease {
    fn drop(&mut self) {
        let mut state = self
            .tails
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if std::thread::panicking() {
            state
                .failure
                .get_or_insert_with(|| "dispatch tail panicked".into());
        } else if self.unfinished.load(std::sync::atomic::Ordering::Acquire) {
            state
                .failure
                .get_or_insert_with(|| "dispatch tail aborted before completion".into());
        }
        state.count -= 1;
        drop(state);
        self.tails.changed();
    }
}
thread_local! {static SYNC_PARENT:RefCell<Vec<DispatchGuard>>=const{RefCell::new(Vec::new())};}
tokio::task_local! {static ASYNC_PARENT:DispatchGuard;}
impl DispatchGuard {
    pub(crate) fn fork(&self) -> Self {
        self.0.tails.acquire()
    }
    pub(crate) fn enter<T>(&self, call: impl FnOnce() -> T) -> T {
        struct Scope;
        impl Drop for Scope {
            fn drop(&mut self) {
                SYNC_PARENT.with(|p| {
                    p.borrow_mut().pop();
                });
            }
        }
        SYNC_PARENT.with(|p| p.borrow_mut().push(self.clone()));
        let _scope = Scope;
        call()
    }
    pub(crate) fn scope<T>(self, future: impl Future<Output = T>) -> impl Future<Output = T> {
        self.0
            .unfinished
            .store(true, std::sync::atomic::Ordering::Release);
        async move {
            let completion = self.clone();
            let result = ASYNC_PARENT.scope(self, future).await;
            completion
                .0
                .unfinished
                .store(false, std::sync::atomic::Ordering::Release);
            result
        }
    }
}
impl DispatchTails {
    fn changed(&self) {
        self.version.send_modify(|n| *n = n.wrapping_add(1));
    }
    fn acquire(self: &Arc<Self>) -> DispatchGuard {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .count += 1;
        self.changed();
        DispatchGuard(Arc::new(Lease {
            tails: self.clone(),
            unfinished: std::sync::atomic::AtomicBool::new(false),
        }))
    }
    /// Registration is tails-only. Never retain this mutex while taking A/R.
    pub(crate) fn register(
        self: &Arc<Self>,
        required: bool,
    ) -> Result<DispatchGuard, RuntimeError> {
        let parent = SYNC_PARENT
            .with(|p| p.borrow().last().cloned())
            .filter(|g| Arc::ptr_eq(&g.0.tails, self))
            .or_else(|| {
                ASYNC_PARENT
                    .try_with(Clone::clone)
                    .ok()
                    .filter(|g| Arc::ptr_eq(&g.0.tails, self))
            });
        if let Some(parent) = parent {
            return Ok(parent.fork());
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed {
            if required {
                state.failure.get_or_insert_with(|| {
                    "unregistered dispatch tail after producer fence".into()
                });
                drop(state);
                self.changed();
            }
            return Err(error("dispatch producers are closed"));
        }
        state.count += 1;
        drop(state);
        self.changed();
        Ok(DispatchGuard(Arc::new(Lease {
            tails: self.clone(),
            unfinished: std::sync::atomic::AtomicBool::new(false),
        })))
    }
    pub(crate) fn try_close(&self) -> bool {
        let mut s = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if s.count != 0 {
            return false;
        }
        s.closed = true;
        drop(s);
        self.changed();
        true
    }
    async fn idle(&self) {
        let mut version = self.version.subscribe();
        loop {
            if self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .count
                == 0
            {
                return;
            }
            if version.changed().await.is_err() {
                return;
            }
        }
    }
    fn result(&self) -> Result<(), RuntimeError> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .failure
            .as_ref()
            .map_or(Ok(()), |reason| Err(error(reason)))
    }
    fn fail(&self, reason: &str) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .failure
            .get_or_insert_with(|| reason.into());
        self.changed();
    }
}
#[derive(Clone, Copy)]
enum FenceKind {
    Shutdown,
    Restart,
}
/// Evidence from a successful producer-stop callback or Checkpointed receipt.
/// Tokens are runtime-bound, reusable, and never manufacture authority.
#[derive(Clone)]
pub struct DispatchDrainFence {
    tails: Arc<DispatchTails>,
    kind: FenceKind,
}
fn error(reason: impl ToString) -> RuntimeError {
    crate::quiesce::runtime::error(reason)
}
impl AgentRuntime {
    pub(crate) fn dispatch_drained(&self) -> bool {
        let state = self
            .shared
            .dispatch_tails
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closed && state.drained && state.failure.is_none()
    }
    pub(crate) fn dispatch_registration_guard(&self) -> Result<DispatchGuard, RuntimeError> {
        self.shared.dispatch_tails.register(false)
    }
    /// Callback reuses the host's existing lane producer/admission/service stop
    /// sequence. Unlike restart, this MUST NOT request safe-point quiescence.
    pub async fn fence_dispatch_shutdown<F, Fut, E>(
        &self,
        stop: F,
    ) -> Result<DispatchDrainFence, RuntimeError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<(), E>>,
        E: std::fmt::Display,
    {
        stop().await.map_err(error)?;
        Ok(DispatchDrainFence {
            tails: self.shared.dispatch_tails.clone(),
            kind: FenceKind::Shutdown,
        })
    }
    pub fn validate_restart_dispatch_fence(
        &self,
        fence: &DispatchDrainFence,
    ) -> Result<(), RuntimeError> {
        if !Arc::ptr_eq(&fence.tails, &self.shared.dispatch_tails)
            || !matches!(fence.kind, FenceKind::Restart)
            || self.quiesce_state() != crate::quiesce::QuiesceState::Checkpointed
        {
            return Err(error("invalid restart dispatch fence"));
        }
        Ok(())
    }
    /// Obtain restart evidence after checkpointing. Once restart drain closes the
    /// registry, only process exit is supported, not operator shutdown fallback.
    /// PR 07 must add a generation-fenced reopen or an explicit close handoff
    /// before it can support such a fallback.
    pub async fn fence_dispatch_restart(&self) -> Result<DispatchDrainFence, RuntimeError> {
        self.quiesce().await?;
        Ok(DispatchDrainFence {
            tails: self.shared.dispatch_tails.clone(),
            kind: FenceKind::Restart,
        })
    }
    /// Drain dispatch tails, independently of runtime.wait. Callers must first
    /// stop ingress. Atomic seal uses A -> R -> tails, not a list snapshot.
    pub async fn drain_dispatch_tails(
        &self,
        fence: &DispatchDrainFence,
    ) -> Result<(), RuntimeError> {
        if !Arc::ptr_eq(&fence.tails, &self.shared.dispatch_tails) {
            return Err(error("foreign dispatch fence"));
        }
        let tails = &self.shared.dispatch_tails;
        loop {
            if matches!(fence.kind, FenceKind::Shutdown) {
                self.drain_lane_admissions().await.map_err(error)?;
                let mut paused = self.paused_recovery_run_ids();
                paused.sort_unstable();
                for run in paused {
                    self.stop(run, crate::StopScope::Subtree)?;
                }
                self.release_shutdown_recovery()?;
                let mut live = self.dispatch_pending_admissions();
                live.extend(
                    self.list_agents()
                        .into_iter()
                        .filter(|r| {
                            matches!(
                                r.phase,
                                event_bus::AgentRunPhase::Pending
                                    | event_bus::AgentRunPhase::Running
                                    | event_bus::AgentRunPhase::Waiting
                            )
                        })
                        .map(|r| r.run_id),
                );
                live.sort();
                live.dedup();
                for run in &live {
                    if let Err(error) = self.stop(*run, crate::StopScope::Subtree)
                        && !self.dispatch_run_settled(*run, &error)
                    {
                        return Err(error);
                    }
                }
                self.join_dispatch_admissions().await?;
                for run in live {
                    match self.wait(run).await {
                        Ok(_) => {}
                        Err(error) if self.dispatch_run_settled(run, &error) => {}
                        Err(e) => return Err(e),
                    }
                }
            }
            tails.idle().await;
            if self.try_seal_dispatch(matches!(fence.kind, FenceKind::Restart)) {
                break;
            }
        }
        if let crate::quiesce::QuiesceState::Failed { reason } = self.quiesce_state() {
            tails.fail(&format!(
                "required durable intent persistence failed: {reason}"
            ));
        }
        tails.result()?;
        let drained = tails
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drained;
        if !drained {
            if let Some(store) = self.shared.run_store.get()
                && store.handle.flush_resume_intents().is_err()
            {
                tails.fail("required dispatch persistence failed");
            }
            tails
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .drained = true;
        }
        tails.result()
    }
}
#[cfg(test)]
#[path = "dispatch_06a_tests.rs"]
mod tests;
