//! A pause point inside the conversation plan write, for tests of a stop around the commit.
//!
//! Only built with the `plan-write-gate` feature, which a test enables through a
//! dev-dependency; no product build contains it. The writer thread blocks at a point the
//! test armed and the test decides how it goes on, so no test waits on time.
//!
//! While the writer is paused it serves nothing else: every `StorageHandle` call blocks,
//! reads included. A test observes the database through its own SQL connection.

use std::cell::RefCell;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use crate::StorageError;

/// Where a plan write can be held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanWritePoint {
    /// Everything is written inside the transaction, which is not committed yet.
    BeforeCommit,
    /// The transaction is committed, the reply is not sent yet.
    AfterCommit,
}

/// What the held write does when it is let go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanWriteResolution {
    /// Carry on as if nothing was held.
    Continue,
    /// Behave as if the process stopped here: before the commit the transaction is rolled
    /// back, after it the commit stays but the caller gets an error instead of the
    /// result, so it cannot know whether the change happened.
    Stop,
}

#[derive(Default)]
struct Inner {
    armed: Option<PlanWritePoint>,
    reached: Option<PlanWritePoint>,
    resolution: Option<PlanWriteResolution>,
}

/// Shared between the test and the writer thread.
#[derive(Default)]
pub struct PlanWriteGate {
    inner: Mutex<Inner>,
    changed: Condvar,
}

impl std::fmt::Debug for PlanWriteGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlanWriteGate").finish_non_exhaustive()
    }
}

impl PlanWriteGate {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The next plan write that reaches `point` is held there. One shot.
    pub fn arm(&self, point: PlanWritePoint) {
        let mut inner = self.lock();
        inner.armed = Some(point);
        inner.resolution = None;
    }

    /// Blocks until a write is held at the armed point, and returns that point.
    pub fn wait_reached(&self) -> PlanWritePoint {
        let mut inner = self.lock();
        loop {
            if let Some(point) = inner.reached {
                return point;
            }
            inner = self.changed.wait(inner).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Lets the held write go on as `resolution` says.
    pub fn resolve(&self, resolution: PlanWriteResolution) {
        let mut inner = self.lock();
        inner.resolution = Some(resolution);
        self.changed.notify_all();
    }

    fn hold(&self, point: PlanWritePoint) -> PlanWriteResolution {
        let mut inner = self.lock();
        if inner.armed != Some(point) {
            return PlanWriteResolution::Continue;
        }
        inner.armed = None;
        inner.reached = Some(point);
        self.changed.notify_all();
        let resolution = loop {
            if let Some(resolution) = inner.resolution.take() {
                break resolution;
            }
            inner = self.changed.wait(inner).unwrap_or_else(|e| e.into_inner());
        };
        inner.reached = None;
        resolution
    }
}

thread_local! {
    static GATE: RefCell<Option<Arc<PlanWriteGate>>> = const { RefCell::new(None) };
}

/// Runs on the writer thread, so the gate belongs to that writer only.
pub(crate) fn install(gate: Arc<PlanWriteGate>) {
    GATE.with(|slot| *slot.borrow_mut() = Some(gate));
}

pub(crate) fn reached(point: PlanWritePoint) -> Result<(), StorageError> {
    let gate = GATE.with(|slot| slot.borrow().clone());
    match gate.map(|gate| gate.hold(point)) {
        Some(PlanWriteResolution::Stop) => Err(StorageError::Io(
            "conversation plan write stopped by the test gate".into(),
        )),
        _ => Ok(()),
    }
}
