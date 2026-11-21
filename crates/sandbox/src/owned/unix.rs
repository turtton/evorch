//! Non-Linux adapter: the existing ownership backend, with no new D-7 claim.
use super::OwnedCommand;
#[derive(Clone, Debug)]
pub struct OwnedGroup(crate::process::ProcessGroup);
impl OwnedGroup {
    pub fn id(&self) -> Option<u32> {
        self.0.leader().map(|pid| pid.as_raw_nonzero().get() as u32)
    }
    pub fn kill(&self) -> io::Result<()> {
        self.0.kill();
        Ok(())
    }
    pub fn terminate(&self) -> io::Result<()> {
        self.0.terminate();
        Ok(())
    }
}
use crate::{
    StdioSession,
    ledger::{Admission, ChildKind},
    process::GroupChild,
};
use std::{
    io,
    process::{ChildStderr, ChildStdin, ChildStdout, ExitStatus},
};
#[derive(Debug)]
pub struct OwnedChild {
    child: GroupChild,
    group: OwnedGroup,
}
impl OwnedChild {
    pub fn spawn(command: &mut OwnedCommand) -> io::Result<Self> {
        let mut raw = command.raw();
        crate::process::apply_launch_policy(&mut raw);
        GroupChild::spawn(&mut raw.into()).map(|child| Self {
            group: OwnedGroup(child.group().clone()),
            child,
        })
    }
    pub fn spawn_in(
        admission: &Admission<'_>,
        kind: ChildKind,
        label: Option<&str>,
        command: &mut OwnedCommand,
    ) -> io::Result<Self> {
        let mut raw = command.raw();
        crate::process::apply_launch_policy(&mut raw);
        admission
            .spawn_group(kind, label, &mut raw.into())
            .map(|child| Self {
                group: OwnedGroup(child.group().clone()),
                child,
            })
    }
    pub fn id(&self) -> Option<u32> {
        self.child.id()
    }
    pub fn group(&self) -> &OwnedGroup {
        &self.group
    }
    pub fn kill(&self) -> io::Result<()> {
        self.child.kill();
        Ok(())
    }
    pub fn take_stdin(&mut self) -> Option<tokio::process::ChildStdin> {
        self.child.take_stdin()
    }
    pub fn take_stdout(&mut self) -> Option<tokio::process::ChildStdout> {
        self.child.take_stdout()
    }
    pub fn take_stderr(&mut self) -> Option<tokio::process::ChildStderr> {
        self.child.take_stderr()
    }
    pub async fn exited(&self) -> io::Result<()> {
        self.child.exited().await
    }
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait().await
    }
    pub async fn kill_and_wait(&mut self) -> io::Result<ExitStatus> {
        self.child.kill_and_wait().await
    }
    pub fn kill_and_reap_blocking(&mut self) -> io::Result<()> {
        self.child.kill_and_reap_blocking()
    }
}
#[derive(Debug)]
pub struct OwnedSession {
    session: StdioSession,
    group: OwnedGroup,
}
impl OwnedSession {
    pub fn spawn(command: &mut OwnedCommand) -> io::Result<Self> {
        Self::spawn_in(&Admission::unmanaged(), ChildKind::Other, None, command)
    }
    pub fn spawn_in(
        admission: &Admission<'_>,
        kind: ChildKind,
        label: Option<&str>,
        command: &mut OwnedCommand,
    ) -> io::Result<Self> {
        StdioSession::spawn_command_in(admission, kind, label, command.raw()).map(|session| Self {
            group: OwnedGroup(session.owned_group().clone()),
            session,
        })
    }
    pub fn id(&self) -> Option<u32> {
        self.session
            .owned_group()
            .leader()
            .map(|pid| pid.as_raw_nonzero().get() as u32)
    }
    pub fn group(&self) -> &OwnedGroup {
        &self.group
    }
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        self.session.take_stdin();
        self.session.wait_for_status()
    }
    pub fn kill_and_wait(&mut self) -> io::Result<ExitStatus> {
        self.group().kill()?;
        self.wait()
    }
    pub fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.session.take_stdin()
    }
    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.session.take_stdout()
    }
    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.session.take_stderr()
    }
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.session.try_wait()
    }
    pub fn output(self, input: Option<&[u8]>) -> io::Result<std::process::Output> {
        self.session.output(input)
    }
}
