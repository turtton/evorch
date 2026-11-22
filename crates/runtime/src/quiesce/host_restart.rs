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
    /// CLI invocations that own work in this core now (see [`HostInvocationLease`]).
    invocations: usize,
    /// A restart passed its invocation preflight and has not reopened the host yet.
    /// While set, no invocation is admitted, even if the phase still reads `Open`.
    restart_claimed: bool,
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
/// Held for as long as one CLI invocation owns work in this core. A planned
/// restart cannot carry a CLI invocation over yet (5b-R), so a held lease
/// refuses the restart before it takes the gate or captures anything.
pub struct HostInvocationLease(HostRestartGate);
/// Why a restart was refused before it began: how many CLI invocations run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvocationBlock {
    pub running: usize,
}
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
        if phase == HostRestartPhase::Open {
            // The restart that claimed admission ended without a successor.
            state.restart_claimed = false;
        }
        self.changed();
        true
    }
    /// Admit one CLI invocation. The decision is made under the same lock as
    /// [`Self::claim_restart_admission`], so an invocation is either counted
    /// before a restart's preflight reads the count or refused after the
    /// restart claimed admission; it is never accepted behind a preflight that
    /// already found none.
    ///
    /// # Errors
    /// [`RuntimeError::RestartBusy`] while the host prepares a restart.
    pub fn enter_invocation(&self) -> Result<HostInvocationLease, RuntimeError> {
        let mut state = self.0.state.lock().unwrap();
        if state.phase != HostRestartPhase::Open || state.restart_claimed {
            return Err(RuntimeError::RestartBusy);
        }
        state.invocations += 1;
        Ok(HostInvocationLease(self.clone()))
    }
    /// CLI invocations running now.
    pub fn running_invocations(&self) -> usize {
        self.0.state.lock().unwrap().invocations
    }
    /// The restart preflight: refuse while a CLI invocation runs, else close
    /// invocation admission until the host reopens (or
    /// [`Self::release_restart_admission`]). Idempotent for one restart.
    ///
    /// # Errors
    /// [`InvocationBlock`] with the number of running invocations.
    pub fn claim_restart_admission(&self) -> Result<(), InvocationBlock> {
        let mut state = self.0.state.lock().unwrap();
        if state.invocations > 0 {
            return Err(InvocationBlock {
                running: state.invocations,
            });
        }
        state.restart_claimed = true;
        Ok(())
    }
    /// A restart that claimed admission failed before it changed the host phase.
    pub fn release_restart_admission(&self) {
        self.0.state.lock().unwrap().restart_claimed = false;
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
        // Request-audit writes can enter Sealing during the awaits after the
        // coordinator's first host drain. Recheck and seal under the same lock
        // as admission; only active writes in this exact attempt are retryable.
        // The coordinator's original preparation deadline bounds this wait.
        let mut changed = self.subscribe();
        loop {
            {
                let mut state = self.0.state.lock().unwrap();
                if state.generation != generation || state.phase != HostRestartPhase::Sealing {
                    return Err(RuntimeError::RestartBusy);
                }
                runtime.validate_restart_dispatch_fence(&fence)?;
                if state.active == 0 {
                    state.phase = HostRestartPhase::Sealed;
                    self.changed();
                    return Ok(SealedRestart {
                        gate: self.clone(),
                        generation,
                        fence,
                    });
                }
            }
            // Subscribe before checking active to retain a concurrent release.
            // Do not spin or shorten the writer callback's permit ownership.
            changed
                .changed()
                .await
                .map_err(|_| RuntimeError::RestartBusy)?;
        }
    }
}
impl Drop for HostInvocationLease {
    fn drop(&mut self) {
        let mut state = self.0.0.state.lock().unwrap();
        state.invocations -= 1;
        self.0.changed();
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
    #[test]
    fn a_restart_claim_and_an_invocation_exclude_each_other() {
        let gate = HostRestartGate::default();
        // An invocation counted first refuses the restart, with its count.
        let first = gate.enter_invocation().unwrap();
        let second = gate.enter_invocation().unwrap();
        assert_eq!(
            gate.claim_restart_admission(),
            Err(InvocationBlock { running: 2 })
        );
        // The refusal closed nothing: invocations are still admitted.
        let third = gate.enter_invocation().unwrap();
        drop((first, second, third));
        assert_eq!(gate.running_invocations(), 0);
        // A claim closes admission although the phase still reads Open ...
        gate.claim_restart_admission().unwrap();
        assert!(gate.enter_invocation().is_err());
        // ... until the claim is released or the host reopens.
        gate.release_restart_admission();
        drop(gate.enter_invocation().unwrap());
        gate.claim_restart_admission().unwrap();
        assert!(gate.transition(1, HostRestartPhase::Open));
        assert!(gate.enter_invocation().is_ok());
        // Nothing is admitted once the host prepares, and a lease is not an
        // accepted command that would hold the seal.
        gate.transition(2, HostRestartPhase::Preparing);
        assert!(gate.enter_invocation().is_err());
    }

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
