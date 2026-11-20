//! Fault points of the audit writes, for tests of a failure around the commit.
//!
//! Only built with the `admin-audit-fault` feature, which a test enables through the
//! dev-dependency on this crate; no product build contains it. The fault belongs to one
//! writer thread. Tests synchronize through their reply callbacks and channels, never
//! through time.

use std::cell::RefCell;
use std::sync::{Arc, Mutex};

use crate::StorageError;

/// Where an armed audit write fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditFaultPoint {
    /// After the work, before the commit: the transaction is rolled back and the caller
    /// gets an error.
    BeforeCommit,
    /// The commit stays but the caller gets an error instead of the result.
    AfterCommitError,
    /// The commit stays and the callback is dropped uncalled, as if the process or the
    /// channel to the caller were gone: an acknowledgement that never arrives.
    AfterCommitDropReply,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AfterCommit {
    Continue,
    Error,
    DropReply,
}

/// Shared between the test and the writer thread.
#[derive(Debug, Default)]
pub struct AuditFault {
    armed: Mutex<Option<(AuditFaultPoint, usize)>>,
}

impl AuditFault {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    /// Fails the audit write that comes after `skip` more writes that reach `point`.
    /// One shot.
    pub fn arm(&self, point: AuditFaultPoint, skip: usize) {
        *self.armed.lock().unwrap_or_else(|e| e.into_inner()) = Some((point, skip));
    }

    /// Whether an armed fault has not fired yet.
    pub fn is_armed(&self) -> bool {
        self.armed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    fn fire(&self, reaching: &[AuditFaultPoint]) -> Option<AuditFaultPoint> {
        let mut armed = self.armed.lock().unwrap_or_else(|e| e.into_inner());
        let (point, skip) = (*armed)?;
        if !reaching.contains(&point) {
            return None;
        }
        if skip == 0 {
            *armed = None;
            Some(point)
        } else {
            *armed = Some((point, skip - 1));
            None
        }
    }
}

thread_local! {
    static FAULT: RefCell<Option<Arc<AuditFault>>> = const { RefCell::new(None) };
}

/// Runs on the writer thread, so the fault belongs to that writer only.
pub(crate) fn install(fault: Arc<AuditFault>) {
    FAULT.with(|slot| *slot.borrow_mut() = Some(fault));
}

fn current() -> Option<Arc<AuditFault>> {
    FAULT.with(|slot| slot.borrow().clone())
}

pub(crate) fn before_commit() -> Result<(), StorageError> {
    match current().and_then(|fault| fault.fire(&[AuditFaultPoint::BeforeCommit])) {
        Some(_) => Err(StorageError::Io(
            "administrative audit write stopped before its commit by the test fault".into(),
        )),
        None => Ok(()),
    }
}

pub(crate) fn after_commit() -> AfterCommit {
    match current().and_then(|fault| {
        fault.fire(&[
            AuditFaultPoint::AfterCommitError,
            AuditFaultPoint::AfterCommitDropReply,
        ])
    }) {
        Some(AuditFaultPoint::AfterCommitError) => AfterCommit::Error,
        Some(AuditFaultPoint::AfterCommitDropReply) => AfterCommit::DropReply,
        _ => AfterCommit::Continue,
    }
}
