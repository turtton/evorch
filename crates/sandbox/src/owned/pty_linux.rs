use super::{OwnedGroup, Owner};
use crate::{
    ledger::{Admission, ChildKind},
    owned::OwnedPtyCommand,
};
pub use process_guard::PtyMaster as OwnedPtyMaster;
use std::{io, process::ExitStatus};

/// Blocking PTY owner. IO never grants retirement authority.
/// Drop uses the shared off-worker reaper; GuardLost stays unconfirmed.
#[derive(Debug)]
pub struct OwnedPtyChild {
    owner: Owner,
}
impl OwnedPtyChild {
    pub fn spawn_in(
        admission: &Admission<'_>,
        kind: ChildKind,
        label: Option<&str>,
        command: &OwnedPtyCommand,
        size: portable_pty::PtySize,
    ) -> io::Result<(Self, OwnedPtyMaster)> {
        let (pending, master) = Self::unregistered(command, size)?;
        Ok((pending.register(admission, kind, label), master))
    }
    fn unregistered(
        command: &OwnedPtyCommand,
        size: portable_pty::PtySize,
    ) -> io::Result<(PendingPty, OwnedPtyMaster)> {
        let (guard, master) = command.target().launch_pty(size)?;
        // Install RAII before any fallible IO setup or ledger operation.
        Ok((PendingPty(Some(Owner::from_guard(guard))), master))
    }
    pub fn id(&self) -> Option<u32> {
        self.group().id()
    }
    pub fn group(&self) -> &OwnedGroup {
        &self.owner.group
    }
    /// Sticky wait; FinalStatus means root reap + group KILL submitted,
    /// not that every descendant has exited or been reaped.
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        self.owner.wait()
    }
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.owner.try_wait()
    }
    pub fn kill_and_wait(&mut self) -> io::Result<ExitStatus> {
        self.group().kill()?;
        self.wait()
    }
}

#[cfg(test)]
#[path = "pty_cancel_tests.rs"]
mod tests;

/// Only lives inside the blocking launch/admission scope. A registration unwind
/// must retire synchronously before releasing that admission; after registration
/// the regular owner can hand cleanup to its off-worker reaper.
struct PendingPty(Option<Owner>);
impl PendingPty {
    fn register(
        mut self,
        admission: &Admission<'_>,
        kind: ChildKind,
        label: Option<&str>,
    ) -> OwnedPtyChild {
        self.0
            .as_mut()
            .expect("pending owner")
            .register(admission, kind, label);
        OwnedPtyChild {
            owner: self.0.take().expect("pending owner"),
        }
    }
}
impl Drop for PendingPty {
    fn drop(&mut self) {
        if let Some(owner) = self.0.as_mut() {
            let _ = owner.group.kill();
            let _ = owner.wait();
        }
    }
}
