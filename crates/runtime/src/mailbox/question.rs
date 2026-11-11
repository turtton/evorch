//! A process-local receipt. The durable question, not a second outbox, is canonical.
use crate::RunId;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug)]
pub(crate) struct QuestionNotice {
    pub(crate) id: String,
    pub(crate) parent: RunId,
    pub(crate) child: RunId,
    pub(crate) blocking: bool,
    observed: AtomicBool,
    pub(crate) admitted: AtomicBool,
    pub(crate) paused: AtomicBool,
}
impl QuestionNotice {
    pub(crate) fn new(id: String, parent: RunId, child: RunId, blocking: bool) -> Self {
        Self {
            id,
            parent,
            child,
            blocking,
            observed: AtomicBool::new(false),
            admitted: AtomicBool::new(false),
            paused: AtomicBool::new(false),
        }
    }
    pub(crate) fn observe(&self) {
        self.observed.store(true, Ordering::Release);
    }
    pub(crate) fn observed(&self) -> bool {
        self.observed.load(Ordering::Acquire)
    }
}
