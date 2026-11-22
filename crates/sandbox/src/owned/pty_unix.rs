//! Existing non-Linux PTY backend. No Linux guard guarantee is claimed here.
use super::{OwnedGroup, OwnedPtyCommand};
use crate::{
    ledger::{Admission, ChildEntry, ChildKind},
    process::ProcessGroup,
};
use std::{io, os::unix::process::ExitStatusExt, process::ExitStatus};

pub struct OwnedPtyMaster(Box<dyn portable_pty::MasterPty + Send>);
impl OwnedPtyMaster {
    pub fn try_clone_reader(&self) -> io::Result<Box<dyn io::Read + Send>> {
        self.0.try_clone_reader().map_err(io::Error::other)
    }
    pub fn take_writer(&self) -> io::Result<Box<dyn io::Write + Send>> {
        self.0.take_writer().map_err(io::Error::other)
    }
    pub fn resize(&self, size: portable_pty::PtySize) -> io::Result<()> {
        self.0.resize(size).map_err(io::Error::other)
    }
}
pub struct OwnedPtyChild {
    child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    group: OwnedGroup,
    entry: Option<ChildEntry>,
    status: Option<ExitStatus>,
}
impl OwnedPtyChild {
    pub fn spawn_in(
        admission: &Admission<'_>,
        kind: ChildKind,
        label: Option<&str>,
        command: &OwnedPtyCommand,
        size: portable_pty::PtySize,
    ) -> io::Result<(Self, OwnedPtyMaster)> {
        let pair = portable_pty::native_pty_system()
            .openpty(size)
            .map_err(io::Error::other)?;
        let (child, group) = ProcessGroup::spawn_pty(&*pair.slave, command.target().pty()?)?;
        let mut owner = Self {
            child: Some(child),
            group: OwnedGroup(group),
            entry: None,
            status: None,
        };
        owner.entry = admission.register_pty_group(kind, label, &owner.group.0);
        Ok((owner, OwnedPtyMaster(pair.master)))
    }
    pub fn id(&self) -> Option<u32> {
        self.group.id()
    }
    pub fn group(&self) -> &OwnedGroup {
        &self.group
    }
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        if let Some(status) = self.status {
            return Ok(status);
        }
        let pid = self
            .group
            .0
            .leader()
            .ok_or_else(|| io::Error::other("PTY leader unavailable without status"))?;
        crate::process::wait_for_exit(pid)?;
        let result = self
            .group
            .0
            .kill_and_reap(|| self.child.as_mut().expect("child").wait().map(Some))?
            .ok_or_else(|| io::Error::other("PTY not reaped"))?;
        if let Some(entry) = self.entry.take() {
            entry.reaped();
        }
        let status = status(result);
        self.status = Some(status);
        Ok(status)
    }
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if self
            .group
            .0
            .leader()
            .map(crate::process::has_exited)
            .transpose()?
            .unwrap_or(true)
        {
            self.wait().map(Some)
        } else {
            Ok(None)
        }
    }
    pub fn kill_and_wait(&mut self) -> io::Result<ExitStatus> {
        self.group.kill()?;
        self.wait()
    }
}
fn status(value: portable_pty::ExitStatus) -> ExitStatus {
    ExitStatus::from_raw((value.exit_code() as i32) << 8)
}
impl Drop for OwnedPtyChild {
    fn drop(&mut self) {
        if self.status.is_some() {
            return;
        }
        self.group.0.kill();
        if let Some(mut child) = self.child.take() {
            let group = self.group.0.clone();
            let entry = self.entry.take();
            let _ = std::thread::Builder::new()
                .name("pty-reaper".into())
                .spawn(move || {
                    if matches!(group.kill_and_reap(|| child.wait().map(Some)), Ok(Some(_)))
                        && let Some(entry) = entry
                    {
                        entry.reaped();
                    }
                });
        }
    }
}
