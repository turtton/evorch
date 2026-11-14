//! Reversible admission for host-owned producers, independent of run/dispatch fences.
//! A permit covers a producer's finite start/commit section, never a run wait.
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

#[derive(Default)]
struct State {
    paused: bool,
    closed: bool,
    active: usize,
    generation: u64,
}
struct Inner {
    identity: u64,
    state: Mutex<State>,
    changed: watch::Sender<u64>,
    admission_changed: watch::Sender<u64>,
}
static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
#[derive(Clone)]
pub struct ProducerGate(Arc<Inner>);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProducerPause(u64, u64);
pub struct ProducerPermit(ProducerGate);
impl Default for ProducerGate {
    fn default() -> Self {
        Self(Arc::new(Inner {
            identity: NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            state: Mutex::default(),
            changed: watch::channel(0).0,
            admission_changed: watch::channel(0).0,
        }))
    }
}
impl ProducerGate {
    fn changed(&self) {
        self.0.changed.send_modify(|v| *v = v.wrapping_add(1));
    }
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.0.changed.subscribe()
    }
    /// Changes only when admission opens/closes, not when a producer drops its own permit.
    pub fn subscribe_admission(&self) -> watch::Receiver<u64> {
        self.0.admission_changed.subscribe()
    }
    fn admission_changed(&self) {
        self.0
            .admission_changed
            .send_modify(|v| *v = v.wrapping_add(1));
        self.changed();
    }
    pub fn try_enter(&self) -> Option<ProducerPermit> {
        let mut state = self.0.state.lock().unwrap();
        if state.paused || state.closed {
            return None;
        }
        state.active += 1;
        Some(ProducerPermit(self.clone()))
    }
    /// Retain the caller's pending payload while paused. Shutdown never resumes it.
    pub async fn enter(&self) -> Option<ProducerPermit> {
        let mut changed = self.subscribe_admission();
        loop {
            if let Some(permit) = self.try_enter() {
                return Some(permit);
            }
            if self.is_closed() {
                return None;
            }
            changed.changed().await.ok()?;
        }
    }
    pub fn pause(&self) -> ProducerPause {
        let mut state = self.0.state.lock().unwrap();
        if !state.paused && !state.closed {
            state.generation = state
                .generation
                .checked_add(1)
                .expect("producer generation exhausted");
            state.paused = true;
            self.admission_changed();
        }
        ProducerPause(self.0.identity, state.generation)
    }
    /// A stale cancellation cannot reopen a later pause or shutdown.
    pub fn resume(&self, pause: ProducerPause) -> bool {
        let mut state = self.0.state.lock().unwrap();
        if state.closed
            || !state.paused
            || self.0.identity != pause.0
            || state.generation != pause.1
        {
            return false;
        }
        state.paused = false;
        self.admission_changed();
        true
    }
    pub fn close(&self) {
        let mut state = self.0.state.lock().unwrap();
        state.closed = true;
        state.paused = true;
        self.admission_changed();
    }
    pub fn is_closed(&self) -> bool {
        self.0.state.lock().unwrap().closed
    }
    pub fn active(&self) -> usize {
        self.0.state.lock().unwrap().active
    }
    pub async fn drained(&self) {
        let mut changed = self.subscribe();
        while self.active() != 0 {
            if changed.changed().await.is_err() {
                break;
            }
        }
    }
}
impl Drop for ProducerPermit {
    fn drop(&mut self) {
        let mut state = self.0.0.state.lock().unwrap();
        state.active -= 1;
        self.0.changed();
    }
}
impl crate::AgentRuntime {
    pub fn producer_gate(&self) -> ProducerGate {
        self.shared.producers.clone()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn permits_do_not_retrigger_admission_schedulers() {
        let gate = ProducerGate::default();
        let mut starts = gate.subscribe_admission();
        let mut activity = gate.subscribe();
        drop(gate.try_enter().unwrap());
        assert!(!starts.has_changed().unwrap());
        assert!(activity.has_changed().unwrap());
        activity.borrow_and_update();
        let a = gate.pause();
        starts.changed().await.unwrap();
        let foreign = ProducerGate::default();
        let foreign_pause = foreign.pause();
        assert!(!gate.resume(foreign_pause));
        assert!(gate.resume(a));
        starts.changed().await.unwrap();
    }
    #[tokio::test]
    async fn default_open_pause_retains_payload_and_stale_resume_cannot_reopen() {
        let gate = ProducerGate::default();
        let accepted = gate.try_enter().unwrap();
        let a = gate.pause();
        let mut pending = Box::pin(gate.enter());
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(
                pending.as_mut().poll(cx).is_pending()
            ))
            .await
        );
        drop(accepted);
        gate.drained().await;
        assert!(gate.resume(a));
        drop(pending.await.unwrap());
        let b = gate.pause();
        assert!(!gate.resume(a));
        assert!(gate.try_enter().is_none());
        gate.close();
        assert!(!gate.resume(b));
        assert!(gate.enter().await.is_none());
    }
}
