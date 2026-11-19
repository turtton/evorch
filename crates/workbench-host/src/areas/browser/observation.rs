//! Read-only source boundary. No PageAgent, CdpTarget, runtime or executor is
//! exposed to consumers. O-S implements source; O-H owns service authorization.
use std::sync::Arc;
use tokio::sync::watch;
use workbench_protocol::{
    ids::{ProjectId, ThreadId},
    namespaces::browser::observation::*,
};
#[cfg(any(test, feature = "test-support"))]
pub mod fake;
pub mod service;
pub mod source;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservationFailure {
    Unavailable,
    Revoked,
    Capacity,
}
/// A single coherent publication: tab identity and pixels must be captured
/// together. A tab switch discards old pixels BEFORE publishing the new tab.
#[derive(Clone)]
pub struct ObservedPage {
    pub snapshot: ObservationSnapshot,
    pub frame: Option<Arc<ObservedFrame>>,
}
#[derive(Clone)]
pub struct ObservedFrame {
    pub start: data::AgentFrameStart,
    pub rgba: Arc<[u8]>,
}
impl ObservedPage {
    pub fn validate(&self) -> bool {
        self.snapshot.validate().is_ok()
            && self.frame.as_ref().is_none_or(|frame| {
                frame.start.validate().is_ok()
                    && frame.start.scope == self.snapshot.scope
                    && self.snapshot.active_tab.as_ref() == Some(&frame.start.tab)
                    && frame.rgba.len() as u64 == frame.start.total_bytes
            })
    }
}
/// latest1 fanout owned by ONE source per execution, never one screencast per
/// subscriber. Revocation/closure wakes waiters; snapshots are non-consuming.
pub struct ObservationFeed {
    pub updates: watch::Receiver<Option<Arc<ObservedPage>>>,
}
pub trait AgentObservationPort: Send + Sync {
    fn list(
        &self,
        project: &ProjectId,
        thread: &ThreadId,
    ) -> Result<Vec<ObservationScope>, ObservationFailure>;
    fn snapshot(&self, scope: &ObservationScope) -> Result<Arc<ObservedPage>, ObservationFailure>;
    fn subscribe(&self, scope: &ObservationScope) -> Result<ObservationFeed, ObservationFailure>;
    /// Must be cheap and authoritative, rechecked before every data transfer.
    fn current(&self, scope: &ObservationScope) -> bool;
}
