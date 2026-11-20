//! The writer side of the administrative audit: a nonblocking submit with a callback
//! reply, and the synchronous form for tests and tools.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{self, TrySendError};

use rusqlite::Connection;

use super::{Command, StorageHandle};
use crate::StorageError;
use crate::admin_audit::{
    AuditCapacity, AuditError, AuditReply, AuditReplyCallback, AuditWrite, store,
};

impl StorageHandle {
    /// Queues an audit command without ever waiting for room. On `Ok` the callback is
    /// called once, on the writer thread, after the command's transaction; if the writer
    /// stops first it is dropped uncalled and the caller must treat the outcome as
    /// unknown and read the operation back. On `Err` nothing was queued and the callback
    /// is dropped: a full queue is [`AuditError::Busy`], a stopped writer is
    /// [`StorageError::WriterClosed`].
    ///
    /// # Errors
    /// See above.
    pub fn try_submit_admin_audit(
        &self,
        write: AuditWrite,
        reply: AuditReplyCallback,
    ) -> Result<(), StorageError> {
        match self.0.try_send(Command::AdminAudit(write, reply)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(AuditError::Busy.into()),
            Err(TrySendError::Disconnected(_)) => Err(StorageError::WriterClosed),
        }
    }

    /// Runs an audit command and waits for its acknowledgement. For tests and tools; an
    /// async caller uses [`Self::try_submit_admin_audit`].
    ///
    /// # Errors
    /// The command's own refusal, or a stopped writer.
    pub fn admin_audit(&self, write: AuditWrite) -> Result<AuditReply, StorageError> {
        let (sender, receiver) = mpsc::channel();
        self.0
            .send(Command::AdminAudit(
                write,
                Box::new(move |result| {
                    let _ = sender.send(result);
                }),
            ))
            .map_err(|_| StorageError::WriterClosed)?;
        receiver.recv().map_err(|_| StorageError::WriterClosed)?
    }

    /// Installs the fault that fails this writer's audit writes at a point the test
    /// arms. Only in builds with the `admin-audit-fault` feature.
    ///
    /// # Errors
    /// A closed writer.
    #[cfg(feature = "admin-audit-fault")]
    pub fn install_admin_audit_fault(
        &self,
        fault: std::sync::Arc<crate::admin_audit::fault::AuditFault>,
    ) -> Result<(), StorageError> {
        self.lane_job(false, move |_| {
            crate::admin_audit::fault::install(fault);
            Ok(())
        })
    }
}

#[cfg(test)]
pub(crate) fn execute(
    database: &crate::Database,
    write: AuditWrite,
) -> Result<AuditReply, StorageError> {
    store::execute(&database.conn, write)
}

/// Runs one command on the writer thread and hands its result to the callback.
pub(super) fn run(
    conn: &Connection,
    writes_suspended: bool,
    write: AuditWrite,
    reply: AuditReplyCallback,
) {
    let mutating = !matches!(write, AuditWrite::Read { .. } | AuditWrite::Stats);
    #[cfg_attr(not(feature = "admin-audit-fault"), allow(unused_mut))]
    let mut result = if writes_suspended && mutating {
        Err(AuditError::Capacity(AuditCapacity::WritesSuspended).into())
    } else {
        store::execute(conn, write)
    };
    #[cfg(feature = "admin-audit-fault")]
    if mutating && result.is_ok() {
        use crate::admin_audit::fault::{AfterCommit, after_commit};
        match after_commit() {
            AfterCommit::Continue => {}
            AfterCommit::DropReply => return,
            AfterCommit::Error => {
                result = Err(StorageError::Io(
                    "administrative audit acknowledgement stopped by the test fault".into(),
                ));
            }
        }
    }
    if catch_unwind(AssertUnwindSafe(|| reply(result))).is_err() {
        tracing::warn!("an administrative audit reply callback panicked");
    }
}
