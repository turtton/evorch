//! Fixed, sealed-ticket-bound deadline; not the 300s preparation target.
use crate::Instance;
use serde::{Deserialize, Serialize};
use std::{
    io,
    sync::{Arc, Condvar, Mutex},
    thread::JoinHandle,
    time::Duration,
};

#[path = "handoff_physical_deadline.rs"]
mod physical_deadline;
pub use physical_deadline::PhysicalAdoptDeadline;

pub const BOOT_TIMEOUT_SECS: u64 = 120;
const LIMIT_NS: u64 = BOOT_TIMEOUT_SECS * 1_000_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootDeadline {
    restarting_ns: u64,
    deadline_ns: u64,
}
impl BootDeadline {
    /// Call once when Restarting is established, before exec preparation.
    pub fn start() -> io::Result<Self> {
        let restarting_ns = monotonic_ns()?;
        Ok(Self {
            restarting_ns,
            deadline_ns: restarting_ns.checked_add(LIMIT_NS).ok_or_else(invalid)?,
        })
    }
    /// Diagnostic elapsed time from the sealed Restarting boundary.
    pub fn elapsed(&self) -> io::Result<Duration> {
        monotonic_ns()?
            .checked_sub(self.restarting_ns)
            .map(Duration::from_nanos)
            .ok_or_else(invalid)
    }
    pub fn restarting_ns(&self) -> u64 {
        self.restarting_ns
    }
    pub fn deadline_ns(&self) -> u64 {
        self.deadline_ns
    }
    pub(super) fn validate(&self) -> io::Result<()> {
        if self.deadline_ns.checked_sub(self.restarting_ns) != Some(LIMIT_NS)
            || monotonic_ns()? < self.restarting_ns
        {
            return Err(invalid());
        }
        Ok(())
    }
}
fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid handoff boot deadline")
}
fn monotonic_ns() -> io::Result<u64> {
    let mut t = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut t) } != 0 {
        return Err(io::Error::last_os_error());
    }
    (t.tv_sec as u64)
        .checked_mul(1_000_000_000)
        .and_then(|n| n.checked_add(t.tv_nsec as u64))
        .ok_or_else(invalid)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BootFailure {
    BeforeAdopt,
    AdoptRejected,
    AdoptAckUnknown,
    /// Includes claim, recovery preparation, endpoint bind and Ready ACK loss.
    AdoptedBeforeReady,
    /// Control Ready was durable, but no activation is allowed after this failure.
    SandboxProbe,
    Interrupt,
    Terminate,
}
impl BootFailure {
    pub fn exit_code(self) -> i32 {
        match self {
            Self::BeforeAdopt => 70,
            Self::AdoptRejected => 71,
            Self::AdoptAckUnknown => 72,
            Self::AdoptedBeforeReady | Self::SandboxProbe => 73,
            Self::Interrupt => 130,
            Self::Terminate => 143,
        }
    }
}
#[derive(Clone, Copy)]
struct State {
    ready: bool,
    failure: BootFailure,
    terminating: bool,
}
impl State {
    fn ready_at(&mut self, now: io::Result<u64>, deadline: u64) -> bool {
        if !self.terminating
            && self.failure == BootFailure::AdoptedBeforeReady
            && now.is_ok_and(|n| n < deadline)
        {
            self.ready = true;
            true
        } else {
            false
        }
    }
}
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

/// Monitor only: no DB, receipt, logging, cleanup or normal shutdown.
/// D owns delivery from the signal inbox and Ready's durable preconditions.
/// Dropping this value does NOT cancel the monitor or release its lifetime FD.
#[must_use]
pub struct BootWatchdog {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    signal_reader: Option<(std::os::unix::net::UnixStream, JoinHandle<()>)>,
    deadline: BootDeadline,
    now: Arc<dyn Fn() -> io::Result<u64> + Send + Sync>,
    instance: Arc<Instance>,
}
impl BootWatchdog {
    pub fn start(instance: Arc<Instance>, deadline: BootDeadline) -> io::Result<Self> {
        deadline.validate()?;
        Self::start_with_clock(instance, deadline, Arc::new(monotonic_ns))
    }
    fn start_with_clock(
        instance: Arc<Instance>,
        deadline: BootDeadline,
        now: Arc<dyn Fn() -> io::Result<u64> + Send + Sync>,
    ) -> io::Result<Self> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                ready: false,
                failure: BootFailure::BeforeAdopt,
                terminating: false,
            }),
            wake: Condvar::new(),
        });
        let child = shared.clone();
        let monitor_clock = now.clone();
        let lifetime = instance.clone();
        let thread = std::thread::Builder::new()
            .name("restart-boot-deadline".into())
            .spawn(move || {
                let _lifetime = lifetime;
                let mut state = child
                    .state
                    .lock()
                    .unwrap_or_else(|_| unsafe { libc::_exit(70) });
                loop {
                    if state.ready {
                        return;
                    }
                    let remaining = monitor_clock()
                        .ok()
                        .and_then(|now| deadline.deadline_ns.checked_sub(now))
                        .filter(|n| *n > 0);
                    let Some(remaining) = remaining else {
                        // No destructors/atexit/IO: blocked writer/fsync/Drop cannot
                        // delay this syscall. The OS closes the inherited OFD.
                        unsafe {
                            libc::_exit(state.failure.exit_code());
                        }
                    };
                    state = child
                        .wake
                        .wait_timeout(state, Duration::from_nanos(remaining))
                        .unwrap_or_else(|_| unsafe { libc::_exit(70) })
                        .0;
                }
            })?;
        Ok(Self {
            shared,
            thread: Some(thread),
            signal_reader: None,
            deadline,
            now,
            instance,
        })
    }
    /// An independent signalfd reader covers blocking lease/migration/writer IO.
    /// Ready and signal consumption share the state lock, so a signal arriving
    /// after Ready remains available to the ordinary lifecycle reader.
    pub fn watch_shutdown(&mut self, inbox: &super::ShutdownInbox) -> io::Result<()> {
        use std::os::fd::{AsFd, AsRawFd};
        if self.signal_reader.is_some() {
            return Err(io::Error::other("boot signal reader already installed"));
        }
        let inbox = super::ShutdownInbox(inbox.0.try_clone()?);
        let (wake, stop) = std::os::unix::net::UnixStream::pair()?;
        let shared = self.shared.clone();
        let thread = std::thread::Builder::new()
            .name("restart-boot-signals".into())
            .spawn(move || {
                let mut fds = [
                    libc::pollfd {
                        fd: inbox.as_fd().as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    },
                    libc::pollfd {
                        fd: stop.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    },
                ];
                loop {
                    let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, -1) };
                    if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    let mut state = shared.state.lock().unwrap();
                    if state.ready || fds[1].revents != 0 {
                        return;
                    }
                    let signals = if n < 0 {
                        Err(io::Error::last_os_error())
                    } else {
                        inbox.read_pending()
                    };
                    if !state.terminating {
                        let failure = match signals {
                            Ok(signals) => signals.first().map(|signal| match signal {
                                super::ShutdownSignal::Interrupt => BootFailure::Interrupt,
                                super::ShutdownSignal::Terminate => BootFailure::Terminate,
                            }),
                            Err(_) => Some(state.failure),
                        };
                        if let Some(failure) = failure {
                            state.failure = failure;
                            state.terminating = true;
                            shared.wake.notify_one();
                            return;
                        }
                    } else {
                        return;
                    }
                }
            })?;
        self.signal_reader = Some((wake, thread));
        Ok(())
    }
    pub fn terminal_failure(&self) -> Option<BootFailure> {
        let state = self.shared.state.lock().unwrap();
        state.terminating.then_some(state.failure)
    }
    /// After DB Adopt readback, before claim/endpoint work.
    pub fn adopted(&self) {
        self.set_stage(BootFailure::AdoptedBeforeReady);
    }
    pub fn ack_unknown(&self) {
        let mut state = self.shared.state.lock().unwrap();
        if !state.ready && !state.terminating && state.failure != BootFailure::AdoptedBeforeReady {
            state.failure = BootFailure::AdoptAckUnknown;
        }
    }
    pub fn shutdown_signal(&self, signal: super::ShutdownSignal) -> bool {
        self.select_failure(match signal {
            super::ShutdownSignal::Interrupt => BootFailure::Interrupt,
            super::ShutdownSignal::Terminate => BootFailure::Terminate,
        })
    }
    fn set_stage(&self, stage: BootFailure) {
        let mut state = self.shared.state.lock().unwrap();
        if !state.ready && !state.terminating {
            state.failure = stage;
        }
    }
    /// First terminal cause wins against other failures and Ready. No IO here.
    pub fn select_failure(&self, failure: BootFailure) -> bool {
        let mut state = self.shared.state.lock().unwrap();
        if state.ready || state.terminating {
            return false;
        }
        state.failure = if state.failure == BootFailure::AdoptedBeforeReady
            && matches!(
                failure,
                BootFailure::BeforeAdopt
                    | BootFailure::AdoptRejected
                    | BootFailure::AdoptAckUnknown
            ) {
            BootFailure::AdoptedBeforeReady
        } else {
            failure
        };
        state.terminating = true;
        self.shared.wake.notify_one();
        true
    }
    /// Only after Ready readback/control bind. Activation and child spawning
    /// remain gated until this succeeds. Equality with the deadline is failure.
    pub fn ready(mut self) -> Result<Arc<Instance>, Self> {
        let accepted = {
            let mut state = self.shared.state.lock().unwrap();
            if state.ready_at((self.now)(), self.deadline.deadline_ns) {
                self.shared.wake.notify_one();
                true
            } else {
                false
            }
        };
        if !accepted {
            return Err(self);
        }
        if let Some(thread) = self.thread.take() {
            thread.join().expect("boot monitor panicked");
        }
        if let Some((mut wake, thread)) = self.signal_reader.take() {
            use std::io::Write;
            let _ = wake.write_all(&[1]);
            thread.join().expect("boot signal reader panicked");
        }
        Ok(self.instance.clone())
    }
    /// The selected cause, including a signal which won the Ready/failure race.
    pub fn selected_failure(&self) -> BootFailure {
        self.shared.state.lock().unwrap().failure
    }
    /// After best-effort receipt/existing-writer diagnostics. Diagnostic IO is
    /// outside this monitor and cannot postpone its deadline.
    pub fn exit_after_diagnostic(&self) -> ! {
        let state = self.shared.state.lock().unwrap();
        unsafe {
            libc::_exit(state.failure.exit_code());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ConfigIdentity, LaunchGuard};
    use std::{
        io::{BufRead, Read, Write},
        process::{Command, Stdio},
        sync::atomic::{AtomicU64, Ordering},
    };

    #[test]
    fn ready_and_deadline_share_exact_boundary_and_failure_latch() {
        for (now, terminal, allowed) in [
            (119, false, true),
            (120, false, false),
            (121, false, false),
            (119, true, false),
        ] {
            let mut state = State {
                ready: false,
                failure: BootFailure::AdoptedBeforeReady,
                terminating: terminal,
            };
            assert_eq!(state.ready_at(Ok(now), 120), allowed);
            assert_eq!(state.ready, allowed);
        }
        let deadline = BootDeadline::start().unwrap();
        assert_eq!(
            deadline.deadline_ns() - deadline.restarting_ns(),
            120_000_000_000
        );
        let mut data = serde_json::to_value(deadline).unwrap();
        data["deadline_ns"] = (deadline.deadline_ns() + 1).into();
        assert!(
            serde_json::from_value::<BootDeadline>(data)
                .unwrap()
                .validate()
                .is_err()
        );
        for failure in [
            BootFailure::BeforeAdopt,
            BootFailure::AdoptAckUnknown,
            BootFailure::AdoptRejected,
        ] {
            let mut state = State {
                ready: false,
                failure,
                terminating: false,
            };
            assert!(!state.ready_at(Ok(0), LIMIT_NS));
        }
    }

    #[test]
    fn ready_transfers_lifetime_to_running_owner() {
        let dir = tempfile::tempdir().unwrap();
        let config = ConfigIdentity::new(dir.path()).unwrap();
        let instance = Arc::new(Instance {
            identity: config.identity().to_owned(),
            config: config.clone(),
            socket: std::path::PathBuf::new(),
            _lifetime: crate::lock(&config.lifetime_path()).unwrap(),
        });
        let monitor = BootWatchdog::start_with_clock(
            instance,
            BootDeadline {
                restarting_ns: 0,
                deadline_ns: LIMIT_NS,
            },
            Arc::new(|| Ok(0)),
        )
        .unwrap();
        monitor.adopted();
        monitor.ack_unknown(); // Ready ACK loss must not downgrade the adopted stage.
        let running = match monitor.ready() {
            Ok(instance) => instance,
            Err(_) => panic!("Adopted before deadline"),
        };
        let launch = LaunchGuard::acquire(&config).unwrap();
        assert!(!launch.lifetime_available().unwrap());
        drop(running);
        assert!(launch.lifetime_available().unwrap());
    }

    // Dedicated subprocess: the real monitor must exit_group even while the
    // bootstrap worker is indefinitely blocked. IPC advances an injected clock,
    // so runner load and wall time are not correctness conditions.
    #[test]
    fn blocked_boot_exits_at_fixed_deadline_without_drops() {
        const CHILD: &str = "EVORCH_UNIT_BOOT_DEADLINE";
        if let Some(code) = std::env::var_os(CHILD) {
            let code: i32 = code.to_str().unwrap().parse().unwrap();
            let config = ConfigIdentity::new(std::path::Path::new(
                &std::env::var_os("EVORCH_UNIT_CONFIG").unwrap(),
            ))
            .unwrap();
            let launch = LaunchGuard::acquire(&config).unwrap();
            // This unit fixture needs a lifetime OFD but no IPC endpoint.
            let instance = Arc::new(Instance {
                identity: config.identity().to_owned(),
                config: config.clone(),
                socket: std::path::PathBuf::new(),
                _lifetime: crate::lock(&config.lifetime_path()).unwrap(),
            });
            drop(launch);
            let clock = Arc::new(AtomicU64::new(0));
            let clock_read = clock.clone();
            let monitor = BootWatchdog::start_with_clock(
                instance,
                BootDeadline {
                    restarting_ns: 0,
                    deadline_ns: LIMIT_NS,
                },
                Arc::new(move || Ok(clock_read.load(Ordering::SeqCst))),
            )
            .unwrap();
            match code {
                70 => (),
                71 => {
                    monitor.select_failure(BootFailure::AdoptRejected);
                }
                72 => monitor.ack_unknown(),
                73 => {
                    monitor.adopted();
                    monitor.ack_unknown();
                    monitor.select_failure(BootFailure::BeforeAdopt);
                }
                130 => {
                    monitor.select_failure(BootFailure::Interrupt);
                }
                143 => {
                    monitor.select_failure(BootFailure::Terminate);
                }
                _ => unreachable!(),
            }
            let shared = monitor.shared.clone();
            // Forgetting the owner's handle must not disable deadline or unlock.
            drop(monitor);
            std::thread::spawn(move || {
                let mut b = [0];
                println!("armed");
                std::io::stdout().flush().unwrap();
                std::io::stdin().read_exact(&mut b).unwrap();
                clock.store(119_000_000_000, Ordering::SeqCst);
                shared.wake.notify_one();
                println!("119");
                std::io::stdout().flush().unwrap();
                std::io::stdin().read_exact(&mut b).unwrap();
                // Synchronize the clock update with the monitor's condition wait.
                let _state = shared.state.lock().unwrap();
                clock.store(LIMIT_NS, Ordering::SeqCst);
                shared.wake.notify_one();
            });
            let (_keep_sender, block) = std::sync::mpsc::channel::<()>();
            block.recv().unwrap();
            unreachable!("blocked bootstrap must not return");
        }
        for code in [70, 71, 72, 73, 130, 143] {
            let dir = tempfile::tempdir().unwrap();
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "handoff::boot::tests::blocked_boot_exits_at_fixed_deadline_without_drops",
                    "--nocapture",
                ])
                .env(CHILD, code.to_string())
                .env("EVORCH_UNIT_CONFIG", dir.path())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap();
            let mut input = child.stdin.take().unwrap();
            let mut output = std::io::BufReader::new(child.stdout.take().unwrap());
            let line = |output: &mut std::io::BufReader<std::process::ChildStdout>,
                        expected: &str| {
                let mut line = String::new();
                loop {
                    assert_ne!(output.read_line(&mut line).unwrap(), 0, "{expected}");
                    if line.trim() == expected {
                        break;
                    }
                    line.clear();
                }
            };
            line(&mut output, "armed");
            input.write_all(b"x").unwrap();
            line(&mut output, "119");
            let identity = ConfigIdentity::new(dir.path()).unwrap();
            let launch = LaunchGuard::acquire(&identity).unwrap();
            assert!(!launch.lifetime_available().unwrap());
            assert!(child.try_wait().unwrap().is_none());
            input.write_all(b"x").unwrap();
            assert_eq!(child.wait().unwrap().code(), Some(code));
            assert!(launch.lifetime_available().unwrap());
        }
    }
}
