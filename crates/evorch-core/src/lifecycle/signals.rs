//! One AsyncFd reader, no detached signal task. Each poll is synchronous.
//! stop() joins its read critical section using the same mutex and transfers
//! the only reader to an opaque receipt (no task/is_finished pseudo-receipt).
use core_instance::handoff::{BlockedShutdownSignals, ShutdownInbox};
use std::{
    io,
    os::fd::AsRawFd,
    sync::{Arc, Mutex},
    task::{Poll, Waker},
};
use tokio::io::unix::AsyncFd;

#[derive(Clone)]
pub struct OperatorSignals(Arc<Mutex<Reader>>);
struct Reader {
    watchdog: Option<super::shutdown_watchdog::Observer>,
    inbox: Option<AsyncFd<ShutdownInbox>>,
    shutdown: bool,
    wake: Option<Waker>,
}
/// Exclusive stopped-reader ownership. Drop restores shutdown handling even
/// after an error/panic and for signals arriving while the reader was stopped.
pub struct StoppedSignals {
    owner: OperatorSignals,
    inbox: Option<AsyncFd<ShutdownInbox>>,
}
impl OperatorSignals {
    /// Install the mask before threads. In a successor, create this only AFTER
    /// inherited-FD inspection. No handler or second signalfd is installed.
    pub fn new(mask: &BlockedShutdownSignals) -> io::Result<Self> {
        Self::from_inbox(mask.inbox()?)
    }
    /// Transfer the bootstrap inbox after its watchdog has stopped.
    pub fn from_inbox(inbox: ShutdownInbox) -> io::Result<Self> {
        Ok(Self(Arc::new(Mutex::new(Reader {
            watchdog: None,
            inbox: Some(AsyncFd::new(inbox)?),
            shutdown: false,
            wake: None,
        }))))
    }
    pub(crate) fn set_shutdown_watchdog(
        &self,
        observer: super::shutdown_watchdog::Observer,
    ) -> io::Result<()> {
        let mut state = self.0.lock().unwrap();
        if state.watchdog.is_some() {
            return Err(io::Error::other("signal watchdog already installed"));
        }
        observer.installed_hook();
        state.watchdog = Some(observer);
        Ok(())
    }
    pub async fn received(&mut self) {
        std::future::poll_fn(|cx| {
            let mut state = self.0.lock().unwrap();
            if state.shutdown {
                return Poll::Ready(());
            }
            state.wake = Some(cx.waker().clone());
            let Reader {
                inbox,
                shutdown,
                watchdog,
                ..
            } = &mut *state;
            let Some(inbox) = inbox.as_ref() else {
                return Poll::Pending;
            };
            let mut ready = match inbox.poll_read_ready(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(_)) => {
                    *shutdown = true;
                    return Poll::Ready(());
                }
                Poll::Ready(Ok(ready)) => ready,
            };
            // Latch before consuming. Only an empty/spurious readiness clears
            // this provisional latch, under the same transfer-excluding mutex.
            *shutdown = true;
            let result = ready.try_io(|inbox| match watchdog {
                Some(observer) => observer.read_pending(inbox.get_ref().as_raw_fd(), || {
                    inbox.get_ref().read_pending()
                }),
                None => inbox.get_ref().read_pending(),
            });
            drop(ready);
            match result {
                Err(_) => {
                    *shutdown = false;
                    Poll::Pending
                }
                Ok(Ok(signals)) if signals.is_empty() => {
                    *shutdown = false;
                    Poll::Pending
                }
                _ => Poll::Ready(()),
            }
        })
        .await
    }
    pub fn stop(&self) -> io::Result<StoppedSignals> {
        let inbox = self
            .0
            .lock()
            .unwrap()
            .inbox
            .take()
            .ok_or_else(|| io::Error::other("signal reader already transferred"))?;
        Ok(StoppedSignals {
            owner: self.clone(),
            inbox: Some(inbox),
        })
    }
}
impl StoppedSignals {
    /// No allocation, logging or signal consumption inside the final latch.
    pub(crate) fn exec_pending(&self) -> io::Result<bool> {
        let state = self.owner.0.lock().unwrap();
        if state.shutdown {
            return Ok(true);
        }
        self.inbox
            .as_ref()
            .expect("owned stopped reader")
            .get_ref()
            .is_pending()
    }

    /// Nonblocking, also covers a shutdown consumed before the transfer.
    pub fn check_pending(&self) -> io::Result<()> {
        let mut state = self.owner.0.lock().unwrap();
        let already_requested = state.shutdown;
        state.shutdown = true;
        let inbox = self.inbox.as_ref().expect("owned stopped reader").get_ref();
        let pending = match &state.watchdog {
            Some(observer) => observer.read_pending(inbox.as_raw_fd(), || inbox.read_pending()),
            None => inbox.read_pending(),
        };
        if matches!(&pending, Ok(signals) if signals.is_empty()) {
            state.shutdown = already_requested;
        } else {
            if let Some(wake) = state.wake.take() {
                wake.wake();
            }
        }
        pending?;
        if state.shutdown {
            Err(io::Error::other("shutdown requested; exec prohibited"))
        } else {
            Ok(())
        }
    }
}
impl Drop for StoppedSignals {
    fn drop(&mut self) {
        let mut state = self.owner.0.lock().unwrap();
        debug_assert!(state.inbox.is_none());
        state.inbox = self.inbox.take();
        if let Some(wake) = state.wake.take() {
            wake.wake();
        }
    }
}
