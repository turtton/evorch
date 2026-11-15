//! Host admission evidence, not an exit/capture authority. No transports are closed.
use runtime::{AgentRuntime, RunId, RuntimeError, StopScope, quiesce::DispatchDrainFence};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HostRestartPhase {
    #[default]
    Open,
    Preparing,
    Sealing,
    Sealed,
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
/// Opaque process-local proof. 07b will add durable transaction/capture authority.
pub struct SealedRestart {
    gate: HostRestartGate,
    generation: u64,
    fence: DispatchDrainFence,
}
impl SealedRestart {
    pub fn validate(&self, runtime: &AgentRuntime) -> Result<(), RuntimeError> {
        let state = self.gate.0.state.lock().unwrap();
        if state.generation != self.generation || state.phase != HostRestartPhase::Sealed {
            return Err(RuntimeError::RestartBusy);
        }
        runtime.validate_restart_dispatch_fence(&self.fence)
    }
}
/// Core installs the single lifecycle coordinator; embedded hosts retain runtime Stop.
pub trait RestartStopHandler: std::any::Any + Send + Sync {
    fn stop(&self, run: RunId, scope: StopScope) -> Result<(), RuntimeError>;
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
