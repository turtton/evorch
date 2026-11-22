//! Test-only gates in the source's task. A test that installs a gate sees the
//! task arrive at lifecycle points and can hold it there; a source without a gate
//! has none of this code, and with the feature off it is not compiled at all.
use super::Inner;
use std::{future::Future, pin::Pin};

/// Where the task of one channel waits for the gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatePoint {
    /// The subscription was accepted and the task admitted; the upstream is not
    /// taken yet.
    BeforeUpstream,
    /// The last receiver left; retirement has not been committed yet.
    NoReceivers,
    /// The task decided to stop and ended its channel (`None` is published);
    /// it is still registered.
    Retiring,
    /// The task left the registry; it has not closed the upstream yet.
    Retired,
}

/// What a test supplies. `wait` is called each time the task reaches `point`
/// and the task continues when the returned future completes.
pub trait SourceGate: Send + Sync {
    fn wait(&self, point: GatePoint) -> Pin<Box<dyn Future<Output = ()> + Send>>;
}

pub(super) async fn pass(inner: &Inner, point: GatePoint) {
    let gate = inner.gate.get().cloned();
    if let Some(gate) = gate {
        gate.wait(point).await;
    }
}
