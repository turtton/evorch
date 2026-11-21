use super::OwnedCommand;
use crate::ledger::{Admission, ChildEntry, ChildKind};
use process_guard::GuardChild;
use std::{
    io,
    process::{ChildStderr, ChildStdin, ChildStdout, ExitStatus},
    sync::{Arc, Mutex, MutexGuard},
};

/// A cloneable control capability. Only the guard sends numeric group signals.
#[derive(Clone, Debug)]
pub struct OwnedGroup(Arc<Mutex<State>>);
#[derive(Debug)]
struct State {
    child: Option<GuardChild>,
    pid: Option<u32>,
    lost: Option<String>,
}
impl OwnedGroup {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn call<T>(&self, f: impl FnOnce(&mut GuardChild) -> io::Result<T>) -> io::Result<T> {
        let mut state = self.lock();
        if let Some(error) = &state.lost {
            return Err(io::Error::other(error.clone()));
        }
        let child = state
            .child
            .as_mut()
            .ok_or_else(|| io::Error::other("owner retired"))?;
        match f(child) {
            Ok(value) => {
                if state.child.as_ref().expect("checked child").is_reaped() {
                    state.pid = None;
                }
                Ok(value)
            }
            Err(error) => {
                // Invalid transport never leaves a live target behind a lost owner.
                let _ = state.child.as_mut().expect("checked child").force_kill();
                state.lost = Some(error.to_string());
                Err(error)
            }
        }
    }
    pub fn id(&self) -> Option<u32> {
        self.lock().pid
    }
    pub fn kill(&self) -> io::Result<()> {
        if self.id().is_none() {
            return Ok(());
        }
        self.call(GuardChild::force_kill)
    }
    /// Normal Stop remains TERM. Core death/force/Drop use KILL.
    pub fn terminate(&self) -> io::Result<()> {
        if self.id().is_none() {
            return Ok(());
        }
        self.call(|child| {
            if child.poll_status()?.is_some() {
                return Ok(());
            }
            child.request_signal(libc::SIGTERM)
        })
    }
    fn status(&self) -> io::Result<Option<ExitStatus>> {
        let status = self.call(GuardChild::poll_status)?;
        if status.is_some() {
            self.lock().pid = None;
        }
        Ok(status)
    }
}

#[derive(Debug)]
pub(super) struct Owner {
    group: OwnedGroup,
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
    entry: Option<ChildEntry>,
}
impl Owner {
    fn spawn(command: &mut OwnedCommand) -> io::Result<Self> {
        let mut child = command.launch()?;
        let pid = child.target_id();
        let stdin = child.take_stdin();
        let stdout = child.take_stdout();
        let stderr = child.take_stderr();
        Ok(Self {
            group: OwnedGroup(Arc::new(Mutex::new(State {
                child: Some(child),
                pid: Some(pid),
                lost: None,
            }))),
            stdin,
            stdout,
            stderr,
            entry: None,
        })
    }
    fn reaped(&mut self) {
        if let Some(entry) = self.entry.take() {
            entry.reaped();
        }
    }
    fn wait(&mut self) -> io::Result<ExitStatus> {
        use std::os::fd::AsRawFd;
        self.group.call(GuardChild::request_wait)?;
        let (control, death) = self
            .group
            .call(|child| Ok((child.control().try_clone()?, child.monitor()?)))?;
        loop {
            if let Some(status) = self.group.status()? {
                self.reaped();
                return Ok(status);
            }
            // Never hold the decoder mutex while waiting: another thread may
            // need the control capability to stop this very target.
            process_guard::wire::poll(
                &mut [
                    process_guard::wire::fd(control.as_raw_fd(), libc::POLLIN),
                    process_guard::wire::fd(death.as_raw_fd(), libc::POLLIN),
                ],
                None,
            )?;
        }
    }
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let status = self.group.call(GuardChild::try_wait)?;
        if status.is_some() {
            self.group.lock().pid = None;
            self.reaped();
        }
        Ok(status)
    }
    fn register(&mut self, admission: &Admission<'_>, kind: ChildKind, label: Option<&str>) {
        self.entry = admission.register_owned(kind, label, &self.group);
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        let _ = self.group.kill();
        let (child, known_lost, finished) = {
            let mut state = self.group.lock();
            let finished = state.pid.is_none() && state.lost.is_none();
            state.pid = None;
            (state.child.take(), state.lost.is_some(), finished)
        };
        if finished {
            self.reaped();
            return;
        }
        let entry = self.entry.take();
        // Never block a Tokio worker. The retained guard owns the target and
        // retires its group before FinalStatus. GuardLost leaves the ledger unconfirmed.
        if let Some(mut child) = child {
            let _ = std::thread::Builder::new()
                .name("guard-reaper".into())
                .spawn(move || {
                    if child.wait().is_ok() && !known_lost {
                        if let Some(entry) = entry {
                            entry.reaped();
                        }
                    }
                });
        }
    }
}

/// Synchronous stdio owner. Drop half-closes control and confirms cleanup by
/// FinalStatus, never by the helper's OS exit code.
#[derive(Debug)]
pub struct OwnedSession {
    owner: Owner,
}
impl OwnedSession {
    pub fn spawn(command: &mut OwnedCommand) -> io::Result<Self> {
        Ok(Self {
            owner: Owner::spawn(command)?,
        })
    }
    pub fn spawn_in(
        admission: &Admission<'_>,
        kind: ChildKind,
        label: Option<&str>,
        command: &mut OwnedCommand,
    ) -> io::Result<Self> {
        let mut session = Self::spawn(command)?;
        session.owner.register(admission, kind, label);
        Ok(session)
    }
    pub fn id(&self) -> Option<u32> {
        self.owner.group.id()
    }
    pub fn group(&self) -> &OwnedGroup {
        &self.owner.group
    }
    pub fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.owner.stdin.take()
    }
    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.owner.stdout.take()
    }
    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.owner.stderr.take()
    }
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.owner.try_wait()
    }
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        self.owner.stdin = None;
        self.owner.wait()
    }
    pub fn kill_and_wait(&mut self) -> io::Result<ExitStatus> {
        self.owner.group.kill()?;
        self.wait()
    }
    pub fn output(mut self, input: Option<&[u8]>) -> io::Result<std::process::Output> {
        use std::io::{Read, Write};
        let stdin = self.take_stdin();
        let stdout = self.take_stdout();
        let stderr = self.take_stderr();
        fn collect(pipe: Option<impl Read>) -> io::Result<Vec<u8>> {
            let mut data = Vec::new();
            if let Some(mut pipe) = pipe {
                pipe.read_to_end(&mut data)?;
            }
            Ok(data)
        }
        std::thread::scope(|scope| {
            let out = scope.spawn(|| collect(stdout));
            let err = scope.spawn(|| collect(stderr));
            let written = match (stdin, input) {
                (Some(mut pipe), Some(input)) => pipe.write_all(input),
                _ => Ok(()),
            };
            let status = self.wait();
            let stdout = out
                .join()
                .map_err(|_| io::Error::other("stdout reader panicked"))??;
            let stderr = err
                .join()
                .map_err(|_| io::Error::other("stderr reader panicked"))??;
            if let Err(error) = written {
                if error.kind() != io::ErrorKind::BrokenPipe {
                    return Err(error);
                }
            }
            Ok(std::process::Output {
                status: status?,
                stdout,
                stderr,
            })
        })
    }
}
impl Drop for OwnedSession {
    fn drop(&mut self) {
        let _ = self.owner.group.kill();
        let _ = self.owner.wait();
    }
}

/// Async owner with decoder/status kept in the owner, not in wait futures.
#[derive(Debug)]
pub struct OwnedChild {
    owner: Owner,
    control: tokio::io::unix::AsyncFd<std::os::unix::net::UnixStream>,
    death: tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>,
    stdin: Option<tokio::process::ChildStdin>,
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Option<tokio::process::ChildStderr>,
}
impl OwnedChild {
    pub fn spawn(command: &mut OwnedCommand) -> io::Result<Self> {
        Self::from_owner(Owner::spawn(command)?)
    }
    fn from_owner(mut owner: Owner) -> io::Result<Self> {
        let (control, death) = owner
            .group
            .call(|child| Ok((child.control().try_clone()?, child.monitor()?)))?;
        Ok(Self {
            control: tokio::io::unix::AsyncFd::new(control)?,
            death: tokio::io::unix::AsyncFd::new(death)?,
            stdin: owner
                .stdin
                .take()
                .map(tokio::process::ChildStdin::from_std)
                .transpose()?,
            stdout: owner
                .stdout
                .take()
                .map(tokio::process::ChildStdout::from_std)
                .transpose()?,
            stderr: owner
                .stderr
                .take()
                .map(tokio::process::ChildStderr::from_std)
                .transpose()?,
            owner,
        })
    }
    pub fn spawn_in(
        admission: &Admission<'_>,
        kind: ChildKind,
        label: Option<&str>,
        command: &mut OwnedCommand,
    ) -> io::Result<Self> {
        let mut owner = Owner::spawn(command)?;
        owner.register(admission, kind, label);
        Self::from_owner(owner)
    }
    pub fn id(&self) -> Option<u32> {
        self.owner.group.id()
    }
    pub fn group(&self) -> &OwnedGroup {
        &self.owner.group
    }
    pub fn kill(&self) -> io::Result<()> {
        self.group().kill()
    }
    pub fn take_stdin(&mut self) -> Option<tokio::process::ChildStdin> {
        self.stdin.take()
    }
    pub fn take_stdout(&mut self) -> Option<tokio::process::ChildStdout> {
        self.stdout.take()
    }
    pub fn take_stderr(&mut self) -> Option<tokio::process::ChildStderr> {
        self.stderr.take()
    }
    async fn ready(&self) -> io::Result<()> {
        tokio::select! {
            event=self.control.readable()=>{event?.clear_ready();},
            event=self.death.readable()=>{event?.clear_ready();},
        }
        Ok(())
    }
    /// Observe root exit without asking for group cleanup or reaping it.
    pub async fn exited(&self) -> io::Result<()> {
        loop {
            if self.group().call(GuardChild::poll_exit)?.is_some() {
                return Ok(());
            }
            self.ready().await?;
        }
    }
    /// Sticky: once polled, cancelling this wait does not cancel the guard's
    /// reservation to kill the group on root exit. Subsequent waits reuse it.
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        self.group().call(GuardChild::request_wait)?;
        loop {
            if let Some(status) = self.group().status()? {
                self.owner.reaped();
                return Ok(status);
            }
            self.ready().await?;
        }
    }
    pub async fn kill_and_wait(&mut self) -> io::Result<ExitStatus> {
        self.kill()?;
        self.wait().await
    }
    pub fn kill_and_reap_blocking(&mut self) -> io::Result<()> {
        self.kill()?;
        self.owner.wait().map(drop)
    }
}
