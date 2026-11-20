//! Planned explicit shutdown, independent of Tokio through runtime destruction.
//! Code 74 does NOT certify cleanup/lease release. Child-process guarantees are
//! a separate follow-up: the expiry thread never locks or kills via a ledger.
use core_instance::handoff::ShutdownInbox;
use std::{
    io,
    os::{
        fd::{AsRawFd, OwnedFd, RawFd},
        unix::net::UnixStream,
    },
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU8, Ordering},
    },
    thread::{self, JoinHandle, Thread},
    time::Instant,
};
use tokio::runtime::Runtime;
mod state;
mod sys;
#[cfg(test)]
pub(crate) mod test_clock;
use state::{COMPLETED, DORMANT, Deadline, EXPIRED};

const CAPTURE: u8 = 1;
const SIGNAL: u8 = 2;
const CAPTURE_READY: u8 = 4;
const HOOK: u8 = 1;
const PUBLISHER: u8 = 2;
const LIFECYCLE: u8 = 4;
const WIRED: u8 = HOOK | PUBLISHER | LIFECYCLE;

#[derive(Clone)]
enum Clock {
    Real(Instant),
    #[cfg(test)]
    Test(Arc<std::sync::atomic::AtomicU64>),
}
impl Clock {
    fn now(&self) -> u64 {
        match self {
            Self::Real(base) => base.elapsed().as_nanos().try_into().unwrap_or(u64::MAX),
            #[cfg(test)]
            Self::Test(now) => now.load(Ordering::Acquire),
        }
    }
    fn timeout(&self, deadline: u64) -> i32 {
        #[cfg(test)]
        if matches!(self, Self::Test(_)) {
            return -1;
        }
        deadline
            .saturating_sub(self.now())
            .div_ceil(1_000_000)
            .min(i32::MAX as u64) as i32
    }
}
struct Shared {
    deadline: Deadline,
    observations: AtomicU8,
    wiring: AtomicU8,
    clock: Clock,
    wake: UnixStream,
    thread: OnceLock<Thread>,
}
impl Shared {
    fn arm_if_eligible(&self, observations: u8) {
        if observations & (CAPTURE | SIGNAL) == (CAPTURE | SIGNAL) {
            self.deadline.arm(self.clock.now());
        }
    }
    fn signal(&self) {
        // One RMW modification order: the second event observes the first.
        let previous = self.observations.fetch_or(SIGNAL, Ordering::AcqRel);
        self.arm_if_eligible(previous | SIGNAL);
    }
    fn notify(&self) {
        // Both waits, including capture-free ordinary shutdown, are covered.
        self.unpark();
        sys::wake(self.wake.as_raw_fd());
    }
    fn unpark(&self) {
        // Linux std's parker uses atomics/futex, not a mutex or fd I/O.
        if let Some(thread) = self.thread.get() {
            thread.unpark();
        }
    }
}

/// Ephemeral observation, never authority to capture, return to Running or exec.
#[derive(Clone)]
pub struct Observer(Arc<Shared>);
impl Observer {
    pub(crate) fn capture_started(&self) {
        let previous = self.0.observations.fetch_or(CAPTURE, Ordering::AcqRel);
        self.0.arm_if_eligible(previous | CAPTURE);
        self.0
            .observations
            .fetch_or(CAPTURE_READY, Ordering::Release);
        // No fd I/O in the capture publication critical section.
        self.0.unpark();
    }
    pub(crate) fn before_read(&self, inbox: RawFd) {
        if sys::pending(inbox) {
            self.0.signal();
            self.0.notify();
        }
    }
    pub(crate) fn read_pending<T>(
        &self,
        inbox: RawFd,
        read: impl FnOnce() -> io::Result<Vec<T>>,
    ) -> io::Result<Vec<T>> {
        self.before_read(inbox);
        let result = read();
        // A signal can arrive AFTER the nonconsuming poll and be consumed by
        // this read. Publish that authoritative observation as well. Repeated
        // publication uses the same AcqRel RMW and never renews the deadline.
        if matches!(&result, Ok(signals) if !signals.is_empty()) {
            self.0.signal();
            self.0.notify();
        }
        result
    }
    pub(crate) fn installed_hook(&self) {
        self.0.wiring.fetch_or(HOOK, Ordering::Release);
    }
    pub(crate) fn installed_publisher(&self) {
        self.0.wiring.fetch_or(PUBLISHER, Ordering::Release);
    }
    pub(crate) fn installed_lifecycle(&self) {
        self.0.wiring.fetch_or(LIFECYCLE, Ordering::Release);
    }
    pub(crate) fn validate_wiring(&self) -> io::Result<()> {
        if self.0.wiring.load(Ordering::Acquire) == WIRED {
            Ok(())
        } else {
            Err(io::Error::other(
                "planned shutdown watchdog wiring incomplete",
            ))
        }
    }
}
struct Watchdog {
    observer: Observer,
    handle: Option<JoinHandle<()>>,
}
impl Watchdog {
    fn prepare(inbox: &impl AsRawFd, clock: Clock) -> io::Result<Self> {
        // Before any new fd could reuse a closed stderr.
        let diagnostic = sys::Diagnostic::prepare();
        let inbox = sys::duplicate(inbox.as_raw_fd())?;
        let (control, wake) = sys::wake_pair()?;
        let shared = Arc::new(Shared {
            deadline: Deadline::new(),
            observations: AtomicU8::new(0),
            wiring: AtomicU8::new(0),
            clock,
            wake,
            thread: OnceLock::new(),
        });
        let (setup, ready) = std::sync::mpsc::sync_channel(1);
        let worker = shared.clone();
        let handle = thread::Builder::new()
            .name("planned-shutdown".into())
            .spawn(move || {
                let _ = worker.thread.set(thread::current());
                let result = sys::prepare_thread_mask();
                let okay = result.is_ok();
                if setup.send(result).is_err() || !okay {
                    return;
                }
                watch(worker, inbox, control, diagnostic);
            })?;
        match ready.recv() {
            Ok(Ok(())) => Ok(Self {
                observer: Observer(shared),
                handle: Some(handle),
            }),
            Ok(Err(error)) => {
                let _ = handle.join();
                Err(error)
            }
            Err(_) => {
                let _ = handle.join();
                Err(io::Error::other("watchdog setup thread stopped"))
            }
        }
    }
    fn finish(&mut self) -> io::Result<()> {
        if self.handle.is_none() {
            return Ok(());
        }
        if !self
            .observer
            .0
            .deadline
            .complete(self.observer.0.clock.now())
        {
            self.observer.0.notify();
            // Due/Expired: do not return, unwind, Drop or exec. The watchdog
            // alone owns fixed diagnostic + _exit; park is not a busy loop.
            loop {
                thread::park();
            }
        }
        self.observer.0.notify();
        self.handle
            .take()
            .unwrap()
            .join()
            .map_err(|_| io::Error::other("shutdown watchdog panicked"))
    }
}
fn watch(shared: Arc<Shared>, inbox: OwnedFd, control: UnixStream, diagnostic: sys::Diagnostic) {
    let mut monitor = Some(inbox.as_raw_fd());
    loop {
        let state = shared.deadline.get();
        if state == COMPLETED {
            return;
        }
        if state == EXPIRED {
            diagnostic.terminate();
        }
        if state != DORMANT {
            if shared.deadline.expire(state, shared.clock.now()) {
                diagnostic.terminate();
            }
            sys::wait(control.as_raw_fd(), None, shared.clock.timeout(state));
            continue;
        }
        let observations = shared.observations.load(Ordering::Acquire);
        if observations & CAPTURE_READY == 0 {
            // park's token plus re-reading atomics on wake prevents lost wake.
            thread::park();
            continue;
        }
        if observations & SIGNAL != 0 {
            shared.arm_if_eligible(observations);
            continue;
        }
        let (pending, failed) = sys::wait(control.as_raw_fd(), monitor, -1);
        if pending {
            shared.signal();
        }
        if failed {
            monitor = None;
        }
    }
}

/// A setup error retains the not-yet-consumed planned boot context.
pub struct SetupFailure<C> {
    pub error: io::Error,
    pub context: C,
}
struct Lifetime {
    runtime: Option<Runtime>,
    watchdog: Watchdog,
}
impl Lifetime {
    fn finish(&mut self) -> io::Result<()> {
        // Also on unwind: no disarm before runtime Drop completes.
        drop(self.runtime.take());
        self.watchdog.finish()
    }
}
impl Drop for Lifetime {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

/// Both product entries and composition tests use this exact outer lifetime.
pub fn run_with_runtime<C, T, E: From<io::Error>>(
    context: C,
    inbox: ShutdownInbox,
    entry: impl FnOnce(C, &Runtime, ShutdownInbox, Observer) -> Result<T, E>,
) -> Result<Result<T, E>, SetupFailure<C>> {
    run(context, inbox, Clock::Real(Instant::now()), entry)
}
fn run<I: AsRawFd, C, T, E: From<io::Error>>(
    context: C,
    inbox: I,
    clock: Clock,
    entry: impl FnOnce(C, &Runtime, I, Observer) -> Result<T, E>,
) -> Result<Result<T, E>, SetupFailure<C>> {
    let mut watchdog = match Watchdog::prepare(&inbox, clock) {
        Ok(watchdog) => watchdog,
        Err(error) => return Err(SetupFailure { error, context }),
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = watchdog.finish();
            return Err(SetupFailure { error, context });
        }
    };
    let mut lifetime = Lifetime {
        runtime: Some(runtime),
        watchdog,
    };
    let observer = lifetime.watchdog.observer.clone();
    let result = entry(
        context,
        lifetime.runtime.as_ref().unwrap(),
        inbox,
        observer.clone(),
    );
    let wiring = if result.is_ok() {
        observer.validate_wiring()
    } else {
        Ok(())
    };
    let finish = lifetime.finish();
    // A receipt error must never overwrite the original error or exit code.
    Ok(result.and_then(|value| {
        wiring?;
        finish?;
        Ok(value)
    }))
}
