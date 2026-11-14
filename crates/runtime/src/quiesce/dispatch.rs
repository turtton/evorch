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
    epoch: u64,
    capture_started: bool,
    capture_invalidated: bool,
    terminal_only: bool,
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
    epoch: u64,
    unfinished: std::sync::atomic::AtomicBool,
}
impl Drop for Lease {
    fn drop(&mut self) {
        let mut state = self
            .tails
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(
            state.epoch, self.epoch,
            "live dispatch lease crossed epochs"
        );
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
        self.0.tails.acquire(self.0.epoch)
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
    fn epoch(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .epoch
    }
    /// Keep live leases and sticky failure. Only a sealed, drained epoch may move.
    pub(crate) fn reopen(&self) -> Result<(), RuntimeError> {
        let mut s = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if s.capture_started {
            return Err(error("CaptureAlreadyStarted"));
        }
        if s.closed {
            if s.count != 0 || !s.drained {
                return Err(error("dispatch not drained"));
            }
            s.epoch = s
                .epoch
                .checked_add(1)
                .ok_or_else(|| error("dispatch epoch exhausted"))?;
            s.closed = false;
            s.drained = false;
        }
        drop(s);
        self.changed();
        Ok(())
    }
    pub(crate) fn capture_started(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .capture_started
    }
    pub(crate) fn capture_valid(&self) -> bool {
        !self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .capture_invalidated
    }
    /// Preserve capture/failure evidence; only terminal descendants may enter.
    pub(crate) fn terminal_handoff(&self) -> Result<(), RuntimeError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed {
            if state.count != 0 {
                return Err(error("live tails in closed dispatch"));
            }
            state.epoch = state
                .epoch
                .checked_add(1)
                .ok_or_else(|| error("dispatch epoch exhausted"))?;
            state.closed = false;
            state.drained = false;
        }
        state.terminal_only = true;
        state.capture_invalidated |= state.capture_started;
        drop(state);
        self.changed();
        Ok(())
    }
    pub(crate) fn register_terminal(self: &Arc<Self>) -> Result<DispatchGuard, RuntimeError> {
        self.register_root(false, true)
    }
    fn changed(&self) {
        self.version.send_modify(|n| *n = n.wrapping_add(1));
    }
    fn acquire(self: &Arc<Self>, epoch: u64) -> DispatchGuard {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(
            state.epoch, epoch,
            "old dispatch guard cannot fork a new epoch"
        );
        state.count += 1;
        drop(state);
        self.changed();
        DispatchGuard(Arc::new(Lease {
            tails: self.clone(),
            epoch,
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
        self.register_root(required, false)
    }
    fn register_root(
        self: &Arc<Self>,
        required: bool,
        terminal: bool,
    ) -> Result<DispatchGuard, RuntimeError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed || (state.terminal_only && !terminal) {
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
        let epoch = state.epoch;
        drop(state);
        self.changed();
        Ok(DispatchGuard(Arc::new(Lease {
            tails: self.clone(),
            epoch,
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
    pub(crate) fn result(&self) -> Result<(), RuntimeError> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .failure
            .as_ref()
            .map_or(Ok(()), |reason| Err(error(reason)))
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
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
    epoch: u64,
    attempt: u64,
}
impl DispatchDrainFence {
    pub(crate) fn is_restart(&self) -> bool {
        self.kind == FenceKind::Restart
    }
}
fn error(reason: impl ToString) -> RuntimeError {
    crate::quiesce::runtime::error(reason)
}
impl AgentRuntime {
    pub(crate) fn seal_dispatch_fence(
        &self,
        fence: &DispatchDrainFence,
    ) -> Result<bool, RuntimeError> {
        let inner = self.shared.quiesce.lock();
        if !Arc::ptr_eq(&fence.tails, &self.shared.dispatch_tails)
            || fence.attempt != inner.generation
            || fence.epoch != self.shared.dispatch_tails.epoch()
            || (fence.is_restart() && inner.phase != crate::quiesce::QuiesceState::Checkpointed)
        {
            return Err(error("stale dispatch sealer"));
        }
        let tails = &self.shared.dispatch_tails;
        if !tails.try_close() {
            return Ok(false);
        }
        let mut state = tails
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let crate::quiesce::QuiesceState::Failed { reason } = &inner.phase {
            state.failure.get_or_insert_with(|| {
                format!("required durable intent persistence failed: {reason}")
            });
        }
        if !state.drained {
            if let Some(store) = self.shared.run_store.get()
                && let Err(error) = store.handle.flush_resume_intents()
            {
                state.failure.get_or_insert_with(|| {
                    format!("required dispatch persistence failed: {error}")
                });
            }
            state.drained = true;
        }
        Ok(true)
    }
    pub(crate) fn validate_dispatch_fence(
        &self,
        fence: &DispatchDrainFence,
    ) -> Result<(), RuntimeError> {
        if !Arc::ptr_eq(&fence.tails, &self.shared.dispatch_tails)
            || fence.epoch != self.shared.dispatch_tails.epoch()
            || fence.attempt != self.shared.quiesce.lock().generation
        {
            return Err(error("stale or foreign dispatch fence"));
        }
        if fence.kind == FenceKind::Restart {
            self.validate_restart_dispatch_fence(fence)?;
        }
        Ok(())
    }
    /// Claims capture under the same boundary as reopen. Capture is irreversible
    /// here; partial writes require the future restart transaction (07b).
    pub fn begin_recovery_capture(&self, fence: &DispatchDrainFence) -> Result<(), RuntimeError> {
        let inner = self.shared.quiesce.lock();
        let mut s = self
            .shared
            .dispatch_tails
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !Arc::ptr_eq(&fence.tails, &self.shared.dispatch_tails)
            || fence.kind != FenceKind::Restart
            || fence.attempt != inner.generation
            || fence.epoch != s.epoch
            || inner.phase != crate::quiesce::QuiesceState::Checkpointed
            || !s.closed
            || !s.drained
            || s.failure.is_some()
            || s.capture_invalidated
        {
            return Err(error("invalid recovery capture fence"));
        }
        s.capture_started = true;
        Ok(())
    }
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
        self.begin_dispatch_shutdown()?;
        stop().await.map_err(error)?;
        Ok(DispatchDrainFence {
            tails: self.shared.dispatch_tails.clone(),
            kind: FenceKind::Shutdown,
            epoch: self.shared.dispatch_tails.epoch(),
            attempt: self.shared.quiesce.lock().generation,
        })
    }
    pub fn validate_restart_dispatch_fence(
        &self,
        fence: &DispatchDrainFence,
    ) -> Result<(), RuntimeError> {
        let inner = self.shared.quiesce.lock();
        if !Arc::ptr_eq(&fence.tails, &self.shared.dispatch_tails)
            || fence.epoch != self.shared.dispatch_tails.epoch()
            || fence.attempt != inner.generation
            || fence.kind != FenceKind::Restart
            || inner.phase != crate::quiesce::QuiesceState::Checkpointed
        {
            return Err(error("invalid restart dispatch fence"));
        }
        Ok(())
    }
    /// Obtain attempt- and epoch-bound evidence after checkpointing.
    pub async fn fence_dispatch_restart(&self) -> Result<DispatchDrainFence, RuntimeError> {
        self.quiesce().await?;
        Ok(DispatchDrainFence {
            tails: self.shared.dispatch_tails.clone(),
            kind: FenceKind::Restart,
            epoch: self.shared.dispatch_tails.epoch(),
            attempt: self.shared.quiesce.lock().generation,
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
        if fence.is_restart() {
            let mut inner = self.shared.quiesce.lock();
            if inner.generation != fence.attempt {
                return Err(error("stale restart sealer"));
            }
            inner.sealing = true;
        }
        loop {
            self.validate_dispatch_fence(fence)?;
            if self.dispatch_drained() {
                return tails.result();
            }
            if matches!(fence.kind, FenceKind::Shutdown) {
                self.drain_lane_admissions().await.map_err(error)?;
                self.shared
                    .dispatch_tails
                    .register_terminal()?
                    .enter(|| self.release_shutdown_recovery())?;
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
            self.validate_dispatch_fence(fence)?;
            if self.try_seal_dispatch(fence)? {
                break;
            }
        }
        self.validate_dispatch_fence(fence)?;
        tails.result()
    }
}
#[cfg(test)]
#[path = "dispatch_06a_tests.rs"]
mod tests;
