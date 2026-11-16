//! Kernel deadline for the single-threaded physical-Adopt interval.
use super::{BootDeadline, BootFailure, BootWatchdog, invalid};
use std::{io, marker::PhantomData, rc::Rc, sync::Arc};

/// No thread, filesystem access, allocation in the handler, or Rust unwinding.
/// SIGALRM is directed only to the entry thread, including during watchdog
/// creation; process-directed delivery could race signal-disposition restoration.
/// Dropping this value deliberately leaves the deadline armed. Only transfer to
/// a watchdog for the *same sealed deadline* may cancel it.
#[must_use]
pub struct PhysicalAdoptDeadline {
    timer: libc::timer_t,
    deadline: BootDeadline,
    previous_action: libc::sigaction,
    previous_mask: libc::sigset_t,
    _entry_thread: PhantomData<Rc<()>>,
}

extern "C" fn expired(_: libc::c_int) {
    // SAFETY: _exit is async-signal-safe. No cleanup can release the inherited
    // lifetime OFD before process exit, even if physical Adopt is stuck in I/O.
    unsafe { libc::_exit(BootFailure::BeforeAdopt.exit_code()) }
}

fn alarm_set() -> libc::sigset_t {
    let mut set = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGALRM);
    }
    set
}
fn mask(how: i32, set: &libc::sigset_t, old: *mut libc::sigset_t) -> io::Result<()> {
    let error = unsafe { libc::pthread_sigmask(how, set, old) };
    if error == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(error))
    }
}
fn absolute_spec(deadline_ns: u64) -> io::Result<libc::itimerspec> {
    Ok(libc::itimerspec {
        it_interval: libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        },
        it_value: libc::timespec {
            tv_sec: (deadline_ns / 1_000_000_000)
                .try_into()
                .map_err(|_| invalid())?,
            tv_nsec: (deadline_ns % 1_000_000_000) as _,
        },
    })
}
fn program(timer: libc::timer_t, spec: &libc::itimerspec) -> io::Result<()> {
    if unsafe { libc::timer_settime(timer, libc::TIMER_ABSTIME, spec, std::ptr::null_mut()) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

impl PhysicalAdoptDeadline {
    /// Call before physical Adopt, while entry is still single-threaded. The
    /// ticket deadline is absolute CLOCK_MONOTONIC, never now + another 120s.
    pub fn start(deadline: BootDeadline) -> io::Result<Self> {
        deadline.validate()?;
        let spec = absolute_spec(deadline.deadline_ns())?;
        let set = alarm_set();
        let mut previous_mask = unsafe { std::mem::zeroed() };
        mask(libc::SIG_BLOCK, &set, &mut previous_mask)?;
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = expired as *const () as usize;
        unsafe { libc::sigemptyset(&mut action.sa_mask) };
        let mut previous_action = unsafe { std::mem::zeroed() };
        if unsafe { libc::sigaction(libc::SIGALRM, &action, &mut previous_action) } != 0 {
            let error = io::Error::last_os_error();
            mask(libc::SIG_SETMASK, &previous_mask, std::ptr::null_mut())?;
            return Err(error);
        }
        let restore = || {
            if unsafe { libc::sigaction(libc::SIGALRM, &previous_action, std::ptr::null_mut()) }
                != 0
            {
                return Err(io::Error::last_os_error());
            }
            mask(libc::SIG_SETMASK, &previous_mask, std::ptr::null_mut())
        };
        let mut event: libc::sigevent = unsafe { std::mem::zeroed() };
        event.sigev_notify = libc::SIGEV_THREAD_ID;
        event.sigev_signo = libc::SIGALRM;
        event.sigev_notify_thread_id = unsafe { libc::gettid() };
        let mut timer = std::ptr::null_mut();
        if unsafe { libc::timer_create(libc::CLOCK_MONOTONIC, &mut event, &mut timer) } != 0 {
            let error = io::Error::last_os_error();
            restore()?;
            return Err(error);
        }
        let guard = Self {
            timer,
            deadline,
            previous_action,
            previous_mask,
            _entry_thread: PhantomData,
        };
        if let Err(error) = program(timer, &spec) {
            guard.cancel();
            return Err(error);
        }
        // A wrapper may have blocked SIGALRM. Explicitly unblock it on the
        // target thread; INT/TERM retain their inherited signalfd-only policy.
        if let Err(error) = mask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut()) {
            guard.cancel();
            return Err(error);
        }
        Ok(guard)
    }

    /// Keep the entry timer armed until the monitor has restored the inherited
    /// signal mask and acknowledged startup. Only then restore the process-wide
    /// disposition and entry mask. Thread creation never blocks entry's SIGALRM.
    pub fn into_watchdog(self, instance: Arc<crate::Instance>) -> io::Result<BootWatchdog> {
        self.into_watchdog_with_clock(instance, Arc::new(super::monotonic_ns))
    }
    fn into_watchdog_with_clock(
        self,
        instance: Arc<crate::Instance>,
        now: Arc<dyn Fn() -> io::Result<u64> + Send + Sync>,
    ) -> io::Result<BootWatchdog> {
        let watchdog =
            BootWatchdog::start_prepared(instance, self.deadline, now, Some(self.previous_mask))?;
        self.transfer_to(&watchdog)?;
        Ok(watchdog)
    }
    fn transfer_to(self, watchdog: &BootWatchdog) -> io::Result<()> {
        if self.deadline != watchdog.deadline {
            return Err(invalid());
        }
        self.cancel();
        Ok(())
    }
    fn cancel(self) {
        // Linux thread-directed notification permits draining on this thread:
        // no queued timer signal can be delivered to a new monitor thread after
        // restoring the previous process-wide disposition.
        let set = alarm_set();
        let result = (|| -> io::Result<()> {
            mask(libc::SIG_BLOCK, &set, std::ptr::null_mut())?;
            if unsafe { libc::timer_delete(self.timer) } != 0 {
                return Err(io::Error::last_os_error());
            }
            let zero = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            loop {
                let signal = unsafe { libc::sigtimedwait(&set, std::ptr::null_mut(), &zero) };
                if signal == libc::SIGALRM {
                    continue;
                }
                let error = io::Error::last_os_error();
                match error.raw_os_error() {
                    Some(libc::EINTR) => continue,
                    Some(libc::EAGAIN) => break,
                    _ => return Err(error),
                }
            }
            if unsafe {
                libc::sigaction(libc::SIGALRM, &self.previous_action, std::ptr::null_mut())
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            mask(libc::SIG_SETMASK, &self.previous_mask, std::ptr::null_mut())
        })();
        if result.is_err() {
            unsafe { libc::_exit(BootFailure::BeforeAdopt.exit_code()) }
        }
    }

    /// Inject monotonic-clock advancement in the physical-stage subprocess
    /// fixture. At the original deadline, expire the real kernel timer instead
    /// of using kill/SIGALRM or waiting for wall time. No production entry uses
    /// this seam, and it cannot change/extend the stored sealed deadline.
    #[cfg(feature = "recovery-test-support")]
    pub fn advance_clock_for_test(&self, now_ns: u64) -> io::Result<()> {
        if now_ns >= self.deadline.deadline_ns() {
            // CLOCK_MONOTONIC absolute 1ns is in the past, not the zero/disarm
            // sentinel. Delivery still exercises timer_create/settime + handler.
            program(self.timer, &absolute_spec(1)?)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn timer_uses_original_absolute_deadline_and_no_repeating_interval() {
        let deadline = BootDeadline {
            restarting_ns: 3_123_456_789,
            deadline_ns: 123_123_456_789,
        };
        let spec = absolute_spec(deadline.deadline_ns()).unwrap();
        assert_eq!(
            (spec.it_value.tv_sec, spec.it_value.tv_nsec),
            (123, 123_456_789)
        );
        assert_eq!((spec.it_interval.tv_sec, spec.it_interval.tv_nsec), (0, 0));
    }
    #[test]
    fn transfer_restores_signal_policy_and_keeps_lifetime_until_ready() {
        const CHILD: &str = "EVORCH_UNIT_PHYSICAL_TRANSFER";
        if let Some(policy) = std::env::var_os(CHILD) {
            let blocked = policy == "blocked";
            let set = alarm_set();
            let mut ignored: libc::sigaction = unsafe { std::mem::zeroed() };
            ignored.sa_sigaction = libc::SIG_IGN;
            unsafe { libc::sigemptyset(&mut ignored.sa_mask) };
            assert_eq!(
                unsafe { libc::sigaction(libc::SIGALRM, &ignored, std::ptr::null_mut()) },
                0
            );
            mask(
                if blocked {
                    libc::SIG_BLOCK
                } else {
                    libc::SIG_UNBLOCK
                },
                &set,
                std::ptr::null_mut(),
            )
            .unwrap();
            let dir = tempfile::tempdir().unwrap();
            let config = crate::ConfigIdentity::new(dir.path()).unwrap();
            let instance = std::sync::Arc::new(crate::Instance {
                identity: config.identity().into(),
                config: config.clone(),
                socket: std::path::PathBuf::new(),
                _lifetime: crate::lock(&config.lifetime_path()).unwrap(),
            });
            let deadline = BootDeadline::start().unwrap();
            let timer = PhysicalAdoptDeadline::start(deadline).unwrap();
            let mut current = unsafe { std::mem::zeroed() };
            let empty = unsafe { std::mem::zeroed() };
            mask(libc::SIG_BLOCK, &empty, &mut current).unwrap();
            assert_eq!(unsafe { libc::sigismember(&current, libc::SIGALRM) }, 0);
            let kernel_timer = timer.timer;
            let (observed, mask_ready) = std::sync::mpsc::channel();
            let watchdog = timer
                .into_watchdog_with_clock(
                    instance,
                    Arc::new(move || {
                        let mut current = unsafe { std::mem::zeroed() };
                        let empty = unsafe { std::mem::zeroed() };
                        mask(libc::SIG_BLOCK, &empty, &mut current)?;
                        observed
                            .send(unsafe { libc::sigismember(&current, libc::SIGALRM) })
                            .unwrap();
                        super::super::monotonic_ns()
                    }),
                )
                .unwrap();
            // This observation is made by the monitor itself, not the entry
            // thread. A temporarily unblocked SIGALRM must not survive there.
            assert_eq!(mask_ready.recv().unwrap(), i32::from(blocked));
            let mut remaining = unsafe { std::mem::zeroed() };
            assert_eq!(
                unsafe { libc::timer_gettime(kernel_timer, &mut remaining) },
                -1
            );
            assert_eq!(
                io::Error::last_os_error().raw_os_error(),
                Some(libc::EINVAL)
            );
            mask(libc::SIG_BLOCK, &empty, &mut current).unwrap();
            assert_eq!(
                unsafe { libc::sigismember(&current, libc::SIGALRM) },
                i32::from(blocked)
            );
            let mut restored: libc::sigaction = unsafe { std::mem::zeroed() };
            assert_eq!(
                unsafe { libc::sigaction(libc::SIGALRM, std::ptr::null(), &mut restored) },
                0
            );
            assert_eq!(restored.sa_sigaction, libc::SIG_IGN);
            mask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut()).unwrap();
            assert_eq!(unsafe { libc::raise(libc::SIGALRM) }, 0);
            watchdog.adopted();
            let running = watchdog
                .ready()
                .unwrap_or_else(|_| panic!("Ready before deadline"));
            let launch = crate::LaunchGuard::acquire(&config).unwrap();
            assert!(!launch.lifetime_available().unwrap());
            drop(running);
            assert!(launch.lifetime_available().unwrap());
            println!("transferred");
            return;
        }
        // Process-wide signal dispositions must not affect parallel unit tests.
        for policy in ["blocked", "unblocked"] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "handoff::boot::physical_deadline::tests::transfer_restores_signal_policy_and_keeps_lifetime_until_ready", "--nocapture"])
                .env(CHILD, policy).output().unwrap();
            assert!(output.status.success(), "{policy}: {output:?}");
            assert!(
                String::from_utf8(output.stdout)
                    .unwrap()
                    .contains("transferred")
            );
        }
    }
}
