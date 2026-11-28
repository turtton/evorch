//! Process-local fence for the sole writer of a live database. A reader takes
//! an optimistic stamp before the SQLite read. A mutation invalidates the
//! stamp before it starts writing, and leaves it unavailable until commit or
//! rollback. A synchronous consumer may use the stamp under the short memory
//! lock; that callback must not perform any I/O or await a writer ACK.
use std::sync::{Arc, Mutex};

#[derive(Debug, Default)]
pub(crate) struct ThreadLedgerClock(Mutex<(u64, bool)>);

#[derive(Clone, Debug)]
pub struct ThreadLedgerObservation {
    clock: Arc<ThreadLedgerClock>,
    generation: u64,
}

impl ThreadLedgerClock {
    pub(crate) fn observe(self: &Arc<Self>) -> Option<ThreadLedgerObservation> {
        let stamp = self.0.lock().ok()?;
        (!stamp.1).then(|| ThreadLedgerObservation {
            clock: self.clone(),
            generation: stamp.0,
        })
    }

    pub(crate) fn write<T>(&self, work: impl FnOnce() -> T) -> T {
        {
            let mut state = self.0.lock().expect("thread ledger clock poisoned");
            assert!(!state.1, "nested thread ledger write");
            state.0 = state
                .0
                .checked_add(1)
                .expect("thread ledger clock exhausted");
            state.1 = true;
        }
        let _writing = WriteGuard(self);
        work()
    }
}

impl ThreadLedgerObservation {
    /// Returns None if SQLite may have observed a changing or superseded ledger.
    pub fn with_current<T>(&self, commit: impl FnOnce() -> T) -> Option<T> {
        let stamp = self.clock.0.lock().ok()?;
        (stamp.0 == self.generation && !stamp.1).then(commit)
    }
}

struct WriteGuard<'a>(&'a ThreadLedgerClock);
impl Drop for WriteGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut state) = self.0.0.lock() {
            state.1 = false;
        }
    }
}

impl crate::StorageHandle {
    pub fn observe_thread_ledger(&self) -> Option<ThreadLedgerObservation> {
        self.thread_ledger_clock().observe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_in_flight_write_invalidates_reads_before_it_can_publish() {
        let clock = Arc::new(ThreadLedgerClock::default());
        let read = clock.observe().unwrap();
        let (started, wait) = std::sync::mpsc::channel();
        let (finish, resume) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                clock.write(move || {
                    started.send(()).unwrap();
                    resume.recv().unwrap();
                })
            });
            wait.recv().unwrap();
            assert!(clock.observe().is_none());
            assert!(
                read.with_current(|| panic!("stale observation used"))
                    .is_none()
            );
            finish.send(()).unwrap();
        });
        assert!(read.with_current(|| ()).is_none());
        assert_eq!(clock.observe().unwrap().with_current(|| 1), Some(1));
    }
}
