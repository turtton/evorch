use rustix::process::{Pid, PidfdFlags, Signal, pidfd_open, pidfd_send_signal};
use serde::Serialize;
use std::{fs, io, os::fd::OwnedFd};
use tokio::io::unix::AsyncFd;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Identity {
    pub pid: i32,
    pub parent: i32,
    pub group: i32,
    pub session: i32,
    pub starttime: u64,
    pub state: char,
}
impl Identity {
    pub fn read(pid: i32) -> io::Result<Self> {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
        // comm can contain both spaces and ')'.
        let tail = stat
            .rsplit_once(") ")
            .ok_or_else(|| io::Error::other("bad proc stat"))?
            .1;
        let fields: Vec<_> = tail.split_whitespace().collect();
        let number = |n: usize| -> io::Result<i32> {
            fields
                .get(n)
                .ok_or_else(|| io::Error::other("short stat"))?
                .parse()
                .map_err(io::Error::other)
        };
        Ok(Self {
            pid,
            parent: number(1)?,
            group: number(2)?,
            session: number(3)?,
            starttime: fields
                .get(19)
                .ok_or_else(|| io::Error::other("short stat"))?
                .parse()
                .map_err(io::Error::other)?,
            state: fields[0].chars().next().unwrap(),
        })
    }
    fn same_process(&self, other: &Self) -> bool {
        // State changes with scheduling and is not identity.
        self.pid == other.pid
            && self.parent == other.parent
            && self.group == other.group
            && self.session == other.session
            && self.starttime == other.starttime
    }
    fn alive(&self) -> bool {
        !matches!(self.state, 'Z' | 'X' | 'x')
    }
}

/// Pin while alive; every signal and exit observation uses this fd, never PID.
pub struct PinnedProcess {
    pub identity: Identity,
    fd: AsyncFd<OwnedFd>,
}
impl PinnedProcess {
    pub fn pin(expected: &Identity) -> io::Result<Self> {
        let pid = Pid::from_raw(expected.pid).ok_or_else(|| io::Error::other("invalid PID"))?;
        let fd = pidfd_open(pid, PidfdFlags::empty())?;
        let actual = Identity::read(expected.pid)?;
        if !actual.same_process(expected) || !actual.alive() {
            return Err(io::Error::other(
                "process changed or died during pidfd_open",
            ));
        }
        Ok(Self {
            identity: actual,
            fd: AsyncFd::new(fd)?,
        })
    }
    pub fn capture(pid: i32) -> io::Result<Self> {
        Self::pin(&Identity::read(pid)?)
    }
    pub fn signal(&self, signal: Signal) -> io::Result<()> {
        match pidfd_send_signal(self.fd.get_ref(), signal) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
    pub async fn exited(&self) -> io::Result<()> {
        let _event = self.fd.readable().await?;
        Ok(())
    }
    pub fn unchanged(&self) -> bool {
        Identity::read(self.identity.pid)
            .is_ok_and(|actual| actual.same_process(&self.identity) && actual.alive())
    }
}
