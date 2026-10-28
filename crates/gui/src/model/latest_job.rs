//! A single background worker that only ever runs the newest request.
//!
//! The thread search modal can be closed and reopened while a storage read is
//! still running. Submitting a job replaces the one waiting to start (dropping
//! it, and with it any channel it holds, which tells its receiver the work was
//! superseded), and at most one job runs at a time.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

struct State<J> {
    waiting: Option<J>,
    /// The worker that owns the right to run jobs, if any. A worker may only
    /// give this up while it is still the owner, so a worker that outlives its
    /// ownership can never release a newer one.
    owner: Option<u64>,
    next_owner: u64,
}

pub struct LatestJob<J> {
    state: Arc<Mutex<State<J>>>,
}

impl<J> Clone for LatestJob<J> {
    fn clone(&self) -> Self {
        Self {
            state: Arc::clone(&self.state),
        }
    }
}

impl<J> Default for LatestJob<J> {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                waiting: None,
                owner: None,
                next_owner: 1,
            })),
        }
    }
}

fn lock<J>(state: &Mutex<State<J>>) -> MutexGuard<'_, State<J>> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Takes the waiting job, or, if there is none, retires `worker` in the same
/// critical section so a job submitted right after cannot be stranded.
fn next_job<J>(state: &Mutex<State<J>>, worker: u64) -> Option<J> {
    let mut state = lock(state);
    let job = state.waiting.take();
    if job.is_none() && state.owner == Some(worker) {
        state.owner = None;
    }
    job
}

/// Gives up `worker`'s ownership unless a newer worker already holds it.
fn abandon<J>(state: &Mutex<State<J>>, worker: u64) {
    let mut state = lock(state);
    if state.owner == Some(worker) {
        state.owner = None;
    }
}

/// Runs when a worker ends, normally or by panic.
struct Abandon<J> {
    state: Arc<Mutex<State<J>>>,
    worker: u64,
}

impl<J> Drop for Abandon<J> {
    fn drop(&mut self) {
        abandon(&self.state, self.worker);
    }
}

impl<J: Send + 'static> LatestJob<J> {
    /// Queues `job`, replacing any job that has not started. `spawn` is called
    /// only when no worker is active, and must run the given loop on a thread.
    pub fn submit(
        &self,
        job: J,
        run: impl Fn(J) + Send + 'static,
        spawn: impl FnOnce(Box<dyn FnOnce() + Send>),
    ) {
        let (replaced, worker) = {
            let mut state = lock(&self.state);
            let replaced = state.waiting.replace(job);
            if state.owner.is_some() {
                drop(state);
                drop(replaced);
                return;
            }
            let worker = state.next_owner;
            state.next_owner += 1;
            state.owner = Some(worker);
            (replaced, worker)
        };
        drop(replaced);
        let guard = Abandon {
            state: Arc::clone(&self.state),
            worker,
        };
        spawn(Box::new(move || {
            while let Some(job) = next_job(&guard.state, guard.worker) {
                run(job);
            }
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::{Receiver, Sender, channel};
    use std::thread::JoinHandle;

    /// A job that reports when it starts and waits until the test releases it.
    struct Probe {
        id: usize,
        started: Sender<usize>,
        release: Receiver<()>,
    }

    #[test]
    fn jobs_run_one_at_a_time_and_only_the_newest_waiting_job_survives() {
        let latest = LatestJob::<Probe>::default();
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let ran = Arc::new(Mutex::new(Vec::new()));
        let workers: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::default();
        let (started_tx, started_rx) = channel();
        let mut releases = Vec::new();
        let submit = |id: usize, releases: &mut Vec<Sender<()>>| {
            let (release_tx, release_rx) = channel();
            releases.push(release_tx);
            let (running, peak, ran) = (running.clone(), peak.clone(), ran.clone());
            let workers = workers.clone();
            latest.submit(
                Probe {
                    id,
                    started: started_tx.clone(),
                    release: release_rx,
                },
                move |job| {
                    let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    ran.lock().unwrap().push(job.id);
                    job.started.send(job.id).unwrap();
                    let _ = job.release.recv();
                    running.fetch_sub(1, Ordering::SeqCst);
                },
                move |worker| workers.lock().unwrap().push(std::thread::spawn(worker)),
            );
        };

        // Given: the first job is running.
        submit(1, &mut releases);
        assert_eq!(started_rx.recv().unwrap(), 1);
        // When: more jobs arrive while it runs, as when a modal is closed and reopened.
        submit(2, &mut releases);
        submit(3, &mut releases);
        // Then: they start no second worker.
        assert_eq!(workers.lock().unwrap().len(), 1);
        // When: the first finishes, only the newest waiting job runs.
        releases[0].send(()).unwrap();
        assert_eq!(started_rx.recv().unwrap(), 3);
        releases[2].send(()).unwrap();
        drop(releases);
        let handles: Vec<_> = workers.lock().unwrap().drain(..).collect();
        for handle in handles {
            handle.join().unwrap();
        }
        // Then: the replaced job never ran and jobs never overlapped.
        assert_eq!(*ran.lock().unwrap(), [1, 3]);
        assert_eq!(peak.load(Ordering::SeqCst), 1);
    }

    type Captured = Arc<Mutex<Vec<Box<dyn FnOnce() + Send>>>>;

    fn capture(workers: &Captured) -> impl FnOnce(Box<dyn FnOnce() + Send>) {
        let workers = workers.clone();
        move |worker| workers.lock().unwrap().push(worker)
    }

    #[test]
    fn a_retired_worker_ending_late_cannot_release_the_worker_that_replaced_it() {
        let latest = LatestJob::<usize>::default();
        let workers: Captured = Arc::default();
        let ran = Arc::new(Mutex::new(Vec::new()));
        let record = |ran: &Arc<Mutex<Vec<usize>>>| {
            let ran = ran.clone();
            move |job| ran.lock().unwrap().push(job)
        };

        // Given: worker A drained its queue and retired, but has not finished ending.
        latest.submit(1, record(&ran), capture(&workers));
        let first = workers.lock().unwrap().remove(0);
        let first_owner = lock(&latest.state).owner.expect("A owns the queue");
        assert_eq!(next_job(&latest.state, first_owner), Some(1));
        assert_eq!(next_job(&latest.state, first_owner), None);
        assert_eq!(lock(&latest.state).owner, None);

        // When: a job arrives and worker B takes ownership before A ends.
        latest.submit(2, record(&ran), capture(&workers));
        assert_eq!(workers.lock().unwrap().len(), 1, "B was spawned");
        let second_owner = lock(&latest.state).owner.expect("B owns the queue");
        assert_ne!(first_owner, second_owner);
        // A finally ends: its release must not touch B's ownership.
        abandon(&latest.state, first_owner);
        assert_eq!(lock(&latest.state).owner, Some(second_owner));

        // Then: a further job queues behind B instead of starting a second worker.
        latest.submit(3, record(&ran), capture(&workers));
        assert_eq!(workers.lock().unwrap().len(), 1);
        let second = workers.lock().unwrap().remove(0);
        second();
        first();
        assert_eq!(*ran.lock().unwrap(), [3]);
    }

    #[test]
    fn a_panicking_job_releases_ownership_so_the_next_job_gets_a_worker() {
        let latest = LatestJob::<usize>::default();
        let workers: Captured = Arc::default();
        latest.submit(1, |_| panic!("storage failure"), capture(&workers));
        let worker = workers.lock().unwrap().remove(0);
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(worker)).is_err());
        latest.submit(2, |_| {}, capture(&workers));
        assert_eq!(workers.lock().unwrap().len(), 1);
    }
}
