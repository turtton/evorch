//! Host admission evidence for the sealed capture boundary. No transports are closed.
use crate::{AgentRuntime, RunId, RuntimeError, quiesce::DispatchDrainFence};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HostRestartPhase {
    #[default]
    Open,
    Preparing,
    Sealing,
    Sealed,
    Capturing,
    Cancelling,
    FailedClosed,
    ShuttingDown,
}
#[derive(Clone, Copy)]
pub enum CommandAccess {
    NewWork,
    ExistingInput,
    Stop,
}
#[derive(Default)]
struct State {
    phase: HostRestartPhase,
    generation: u64,
    active: usize,
}
struct Inner {
    state: Mutex<State>,
    changed: watch::Sender<u64>,
}
#[derive(Clone)]
pub struct HostRestartGate(Arc<Inner>);
/// Clones share one accepted command; synchronous reply and async persistence must both finish.
pub struct HostCommandPermit(Arc<HostCommandLease>);
struct HostCommandLease(HostRestartGate);
impl Clone for HostCommandPermit {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl Default for HostRestartGate {
    fn default() -> Self {
        Self(Arc::new(Inner {
            state: Mutex::default(),
            changed: watch::channel(0).0,
        }))
    }
}
impl HostRestartGate {
    pub fn same_instance(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    fn changed(&self) {
        self.0.changed.send_modify(|v| *v = v.wrapping_add(1));
    }
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.0.changed.subscribe()
    }
    pub fn phase(&self) -> HostRestartPhase {
        self.0.state.lock().unwrap().phase
    }
    pub fn busy(&self) -> bool {
        // Shutdown itself owns process lifetime; completed host drains are not new busy work.
        !matches!(
            self.phase(),
            HostRestartPhase::Open | HostRestartPhase::ShuttingDown
        )
    }
    /// The coordinator serializes transitions. Expected generation rejects old completions.
    pub fn transition(&self, generation: u64, phase: HostRestartPhase) -> bool {
        let mut state = self.0.state.lock().unwrap();
        if generation < state.generation
            || state.phase == HostRestartPhase::ShuttingDown
            || (state.phase == HostRestartPhase::FailedClosed
                && !matches!(
                    phase,
                    HostRestartPhase::FailedClosed | HostRestartPhase::ShuttingDown
                ))
        {
            return false;
        }
        state.generation = generation;
        state.phase = phase;
        self.changed();
        true
    }
    pub fn enter(&self, access: CommandAccess) -> Result<HostCommandPermit, RuntimeError> {
        let mut state = self.0.state.lock().unwrap();
        let allowed = state.phase == HostRestartPhase::Open
            || matches!(access, CommandAccess::Stop)
            || (state.phase == HostRestartPhase::Preparing
                && matches!(access, CommandAccess::ExistingInput));
        if !allowed {
            return Err(RuntimeError::RestartBusy);
        }
        state.active += 1;
        Ok(HostCommandPermit(Arc::new(HostCommandLease(self.clone()))))
    }
    /// Short request-audit admission, atomic with sealing. Unlike background
    /// producers this remains available during Preparing. No admission after a
    /// sealed proof is minted: capture can never overtake an accepted write.
    ///
    /// The writer callback owns this until completion or command drop. A 10s
    /// caller ACK budget must not release an in-flight write across capture.
    /// The restart driver itself never holds it; cancel/status never wait for it.
    pub fn enter_admin_audit(&self) -> Result<HostCommandPermit, RuntimeError> {
        let mut state = self.0.state.lock().unwrap();
        if !matches!(
            state.phase,
            HostRestartPhase::Open
                | HostRestartPhase::Preparing
                | HostRestartPhase::Sealing
                | HostRestartPhase::Cancelling
        ) {
            return Err(RuntimeError::RestartBusy);
        }
        state.active += 1;
        Ok(HostCommandPermit(Arc::new(HostCommandLease(self.clone()))))
    }
    pub async fn drained(&self) {
        let mut changed = self.subscribe();
        loop {
            if self.0.state.lock().unwrap().active == 0 {
                return;
            }
            if changed.changed().await.is_err() {
                return;
            }
        }
    }
    /// Mint only after host admission and runtime dispatch have both drained.
    pub async fn sealed(
        &self,
        runtime: &AgentRuntime,
        generation: u64,
        fence: DispatchDrainFence,
    ) -> Result<SealedRestart, RuntimeError> {
        runtime.drain_dispatch_tails(&fence).await?;
        let mut state = self.0.state.lock().unwrap();
        if state.generation != generation
            || state.phase != HostRestartPhase::Sealing
            || state.active != 0
        {
            return Err(RuntimeError::RestartBusy);
        }
        runtime.validate_restart_dispatch_fence(&fence)?;
        state.phase = HostRestartPhase::Sealed;
        self.changed();
        Ok(SealedRestart {
            gate: self.clone(),
            generation,
            fence,
        })
    }
}
impl Drop for HostCommandLease {
    fn drop(&mut self) {
        let mut state = self.0.0.state.lock().unwrap();
        state.active -= 1;
        self.0.changed();
    }
}
/// Opaque proof that host admission and runtime dispatch have both drained.
pub struct SealedRestart {
    gate: HostRestartGate,
    generation: u64,
    fence: DispatchDrainFence,
}
impl SealedRestart {
    /// Call under the lifecycle coordinator mutex; publish Capturing in that
    /// same synchronous section. No database write or await occurs here.
    pub fn begin_capture(
        self,
        runtime: &AgentRuntime,
        tx: &storage::recovery::restart::RestartTransaction,
        recipes: std::collections::BTreeMap<RunId, crate::quiesce::RecoveryRecipe>,
    ) -> Result<crate::RestartCaptureSession, RuntimeError> {
        runtime.begin_restart_capture_session(self, tx, recipes)
    }
    pub(crate) fn into_fence(
        self,
        runtime: &AgentRuntime,
    ) -> Result<DispatchDrainFence, RuntimeError> {
        self.validate(runtime)?;
        Ok(self.fence)
    }
    pub fn validate(&self, runtime: &AgentRuntime) -> Result<(), RuntimeError> {
        let state = self.gate.0.state.lock().unwrap();
        if state.generation != self.generation || state.phase != HostRestartPhase::Sealed {
            return Err(RuntimeError::RestartBusy);
        }
        runtime.validate_restart_dispatch_fence(&self.fence)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn preparing_admits_only_existing_input_then_sealing_drains_accepted_commands() {
        let gate = HostRestartGate::default();
        let accepted = gate.enter(CommandAccess::NewWork).unwrap();
        assert!(gate.transition(1, HostRestartPhase::Preparing));
        assert!(gate.enter(CommandAccess::NewWork).is_err());
        assert!(gate.enter(CommandAccess::ExistingInput).is_ok());
        gate.transition(1, HostRestartPhase::Sealing);
        assert!(gate.enter(CommandAccess::ExistingInput).is_err());
        let mut drained = Box::pin(gate.drained());
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(drained.as_mut().poll(cx)))
                .await
                .is_pending()
        );
        drop(accepted);
        drained.await;
        gate.transition(1, HostRestartPhase::FailedClosed);
        assert!(gate.enter(CommandAccess::Stop).is_ok());
        assert!(!gate.transition(2, HostRestartPhase::Open));
        assert!(gate.busy());
        gate.transition(2, HostRestartPhase::ShuttingDown);
        assert!(!gate.transition(3, HostRestartPhase::Open));
    }
}
