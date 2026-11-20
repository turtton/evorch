//! Crate-private, cfg(test)-only seam. Called only by restart's dedicated tests.
//! Synthetic fd/observations below are unit tests, NOT production-composition
//! evidence. In particular they do not fabricate a ShutdownInbox/private token.
use super::*;
use std::sync::atomic::AtomicU64;

pub(crate) const DORMANT_STATE: u64 = DORMANT;
pub(crate) const COMPLETED_STATE: u64 = COMPLETED;
pub(crate) const EXPIRED_STATE: u64 = EXPIRED;
#[derive(Clone, Default)]
pub(crate) struct TestClock(Arc<AtomicU64>);
impl TestClock {
    pub(crate) fn advance_to(&self, now: u64, observer: &Observer) {
        assert!(now >= self.0.load(Ordering::Acquire));
        self.0.store(now, Ordering::Release);
        observer.0.notify();
    }
    pub(crate) fn run_with_inbox<C, T>(
        &self,
        context: C,
        inbox: ShutdownInbox,
        entry: impl FnOnce(C, &Runtime, ShutdownInbox, Observer) -> io::Result<T>,
    ) -> Result<io::Result<T>, SetupFailure<C>> {
        super::run(context, inbox, Clock::Test(self.0.clone()), entry)
    }
    pub(crate) fn run_invalid_inbox<C>(
        &self,
        context: C,
    ) -> Result<io::Result<()>, SetupFailure<C>> {
        struct Invalid;
        impl AsRawFd for Invalid {
            fn as_raw_fd(&self) -> RawFd {
                -1
            }
        }
        super::run(
            context,
            Invalid,
            Clock::Test(self.0.clone()),
            |_, _, _, _| panic!("entry must not run when mandatory descriptor preparation fails"),
        )
    }
    pub(crate) fn observer(&self) -> Observer {
        let (_reader, wake) = sys::wake_pair().unwrap();
        Observer(Arc::new(Shared {
            deadline: Deadline::new(),
            observations: AtomicU8::new(0),
            wiring: AtomicU8::new(0),
            clock: Clock::Test(self.0.clone()),
            wake,
            thread: OnceLock::new(),
        }))
    }
    pub(crate) fn run<C, T>(
        &self,
        context: C,
        entry: impl FnOnce(C, &Runtime, Observer) -> io::Result<T>,
    ) -> Result<io::Result<T>, SetupFailure<C>> {
        // No process-directed signal/exec is used in these synthetic units.
        // Only the test thread and its newly created descendants need this mask.
        sys::block_test_signals().unwrap();
        let (inbox, _peer) = sys::wake_pair().unwrap();
        super::run(
            context,
            inbox,
            Clock::Test(self.0.clone()),
            |context, runtime, _inbox, observer| entry(context, runtime, observer),
        )
    }
}
pub(crate) fn signal(observer: &Observer) {
    observer.0.signal();
    observer.0.notify();
}
pub(crate) fn deadline(observer: &Observer) -> u64 {
    observer.0.deadline.get()
}
pub(crate) struct Decision(Deadline);
impl Decision {
    pub(crate) fn new() -> Self {
        Self(Deadline::new())
    }
    pub(crate) fn arm(&self, now: u64) {
        self.0.arm(now);
    }
    pub(crate) fn get(&self) -> u64 {
        self.0.get()
    }
    pub(crate) fn complete(&self, now: u64) -> bool {
        self.0.complete(now)
    }
    pub(crate) fn expire(&self, now: u64) -> bool {
        self.0.expire(self.0.get(), now)
    }
}

pub(crate) fn wire(observer: &Observer) {
    observer.installed_hook();
    observer.installed_publisher();
    observer.installed_lifecycle();
}
pub(crate) fn poll_unit(fd: RawFd) -> (bool, bool, bool) {
    let (control, _peer) = sys::wake_pair().unwrap();
    let (pending, failed) = sys::wait(control.as_raw_fd(), Some(fd), 0);
    (sys::pending(fd), pending, failed)
}
