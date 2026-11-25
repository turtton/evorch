//! In-process persistence notifications. Not part of the event wire schema.
use crate::Event;
use std::sync::{Arc, OnceLock};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PersistenceStage {
    Published,
    Committed,
    Failed,
}
pub type PersistenceObserver = dyn Fn(&Event, PersistenceStage) + Send + Sync;
/// A leaf callback: implementations must not perform I/O, wait, or acquire other locks.
#[derive(Default)]
pub struct PersistenceNotifications(OnceLock<Arc<PersistenceObserver>>);
impl PersistenceNotifications {
    pub fn install(&self, observer: Arc<PersistenceObserver>) -> bool {
        self.0.set(observer).is_ok()
    }
    pub fn notify(&self, event: &Event, stage: PersistenceStage) {
        if let Some(observer) = self.0.get() {
            observer(event, stage);
        }
    }
}
