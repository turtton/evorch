//! Display-only knowledge of which threads this process owns, for threads the
//! shared feed shows as running. Never use it to authorize an operation.
//!
//! Reading the registry is a synchronous SQL query, so frames never do it:
//! a background worker probes the candidate threads and the frame reads the
//! last answer. A probe that fails or finds nothing conclusive leaves a thread
//! unmarked, since "unknown" is no evidence that another process works on it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use runtime::ownership::{OwnerHost, RegistryError, ThreadOwner};

/// Re-probe cadence while candidates exist; an ownership event or a changed
/// candidate set probes sooner.
const RECHECK: Duration = Duration::from_secs(2);

/// Reads a thread's owner record. Production reads the registry without
/// claiming or renewing anything.
pub(super) type Reader = Arc<dyn Fn(&str) -> Result<ThreadOwner, RegistryError> + Send + Sync>;

/// Who this process is, and how to read who owns a thread.
#[derive(Clone)]
pub(super) struct Source {
    pub owner_id: String,
    pub read: Reader,
}

impl Source {
    pub(super) fn of_host(host: &Arc<OwnerHost>) -> Self {
        let reader = Arc::clone(host);
        Self {
            owner_id: host.owner_id().to_owned(),
            read: Arc::new(move |thread| reader.attach(thread)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    OwnedHere,
    NotOwnedHere,
}

/// Whose thread the record says it is. The holder alone decides: a state such
/// as Quiescing or Released, which refuses a new turn, is still this process's
/// thread. A missing record means nobody holds it; any other failure proves
/// nothing.
fn classify(owner_id: &str, record: Result<ThreadOwner, RegistryError>) -> Option<Verdict> {
    match record {
        Ok(owner) if owner.lease.owner_id == owner_id => Some(Verdict::OwnedHere),
        Ok(_) | Err(RegistryError::Absent) => Some(Verdict::NotOwnedHere),
        Err(_) => None,
    }
}

struct Request {
    revision: u64,
    source: Source,
    threads: Vec<String>,
    repaint: Option<egui::Context>,
}

/// What a probe found, tagged with the revision it was asked under.
struct Answer {
    revision: u64,
    verdicts: Vec<(String, Option<Verdict>)>,
}

struct Worker {
    requests: mpsc::SyncSender<Request>,
    answers: mpsc::Receiver<Answer>,
}

impl Worker {
    fn start() -> std::io::Result<Self> {
        let (requests, receiver) = mpsc::sync_channel::<Request>(1);
        let (sender, answers) = mpsc::sync_channel::<Answer>(1);
        std::thread::Builder::new()
            .name("external-work-display".into())
            .spawn(move || {
                while let Ok(request) = receiver.recv() {
                    let verdicts = request
                        .threads
                        .into_iter()
                        .map(|thread| {
                            let verdict =
                                classify(&request.source.owner_id, (request.source.read)(&thread));
                            (thread, verdict)
                        })
                        .collect();
                    let answer = Answer {
                        revision: request.revision,
                        verdicts,
                    };
                    if sender.send(answer).is_err() {
                        break;
                    }
                    if let Some(ctx) = request.repaint {
                        ctx.request_repaint();
                    }
                }
            })?;
        Ok(Self { requests, answers })
    }
}

pub(super) struct ExternalWork {
    source: Option<Source>,
    recheck: Duration,
    worker: Option<Worker>,
    verdicts: BTreeMap<String, Verdict>,
    /// The candidates the last request asked about.
    wanted: BTreeSet<String>,
    /// Bumped by every ownership change; an answer asked under an older one
    /// describes a registry that no longer exists and is dropped.
    revision: u64,
    in_flight: bool,
    last_request: Option<Instant>,
    invalidated: bool,
}

impl Default for ExternalWork {
    fn default() -> Self {
        Self {
            source: None,
            recheck: RECHECK,
            worker: None,
            verdicts: BTreeMap::new(),
            wanted: BTreeSet::new(),
            revision: 0,
            in_flight: false,
            last_request: None,
            invalidated: false,
        }
    }
}

impl ExternalWork {
    pub(super) fn set_source(&mut self, source: Source) {
        self.source = Some(source);
        self.invalidate();
    }

    #[cfg(test)]
    pub(super) fn with_recheck(mut self, recheck: Duration) -> Self {
        self.recheck = recheck;
        self
    }

    /// Ownership changed somewhere. Every earlier answer, here or still on its
    /// way, stops counting at once, so a thread this process just took is
    /// never shown as another process's while the next probe runs.
    pub(super) fn invalidate(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        self.invalidated = true;
        self.verdicts.clear();
    }

    /// Whether the last answer says another party holds `thread`, or nobody does.
    pub(super) fn not_owned_here(&self, thread: &str) -> bool {
        match &self.source {
            None => true,
            Some(_) => self.verdicts.get(thread) == Some(&Verdict::NotOwnedHere),
        }
    }

    /// Picks up a finished probe and, when one is due, hands the candidates to
    /// the worker. Never reads the registry itself and never waits.
    pub(super) fn refresh(
        &mut self,
        candidates: &BTreeSet<String>,
        repaint: Option<&egui::Context>,
    ) {
        self.collect();
        self.verdicts
            .retain(|thread, _| candidates.contains(thread));
        if candidates.is_empty() {
            self.wanted.clear();
            return;
        }
        let Some(source) = self.source.clone() else {
            return;
        };
        let due = self.invalidated
            || self.wanted != *candidates
            || self
                .last_request
                .is_none_or(|at| at.elapsed() >= self.recheck);
        if due && !self.in_flight {
            self.request(source, candidates, repaint);
        }
        if let Some(ctx) = repaint {
            ctx.request_repaint_after(self.recheck);
        }
    }

    fn request(
        &mut self,
        source: Source,
        candidates: &BTreeSet<String>,
        repaint: Option<&egui::Context>,
    ) {
        if self.worker.is_none() {
            match Worker::start() {
                Ok(worker) => self.worker = Some(worker),
                Err(error) => {
                    tracing::warn!(%error, "work in other windows cannot be marked");
                    return;
                }
            }
        }
        let Some(worker) = &self.worker else {
            return;
        };
        let request = Request {
            source,
            threads: candidates.iter().cloned().collect(),
            repaint: repaint.cloned(),
            revision: self.revision,
        };
        if worker.requests.try_send(request).is_ok() {
            self.in_flight = true;
            self.invalidated = false;
            self.wanted = candidates.clone();
            self.last_request = Some(Instant::now());
        }
    }

    fn collect(&mut self) {
        let Some(worker) = &self.worker else {
            return;
        };
        let answers: Vec<Answer> = std::iter::from_fn(|| worker.answers.try_recv().ok()).collect();
        for answer in answers {
            self.apply(answer);
        }
    }

    fn apply(&mut self, answer: Answer) {
        self.in_flight = false;
        if answer.revision != self.revision {
            return;
        }
        for (thread, verdict) in answer.verdicts {
            match verdict {
                Some(verdict) => {
                    self.verdicts.insert(thread, verdict);
                }
                None => {
                    self.verdicts.remove(&thread);
                }
            }
        }
    }

    /// Whether a probe has been asked and not yet taken in.
    pub(super) fn probing(&self) -> bool {
        self.in_flight
    }

    /// Blocks until the probe in flight has answered. For headless runs that
    /// have no frames to come back to; a frame never calls it.
    pub(super) fn wait_idle(&mut self) {
        if !self.in_flight {
            return;
        }
        let answer = self
            .worker
            .as_ref()
            .and_then(|worker| worker.answers.recv().ok());
        match answer {
            Some(answer) => self.apply(answer),
            None => self.in_flight = false,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    use runtime::ownership::{Lease, OwnerState};

    use super::*;

    const ME: &str = "owner-me";

    fn record(owner_id: &str, state: OwnerState) -> ThreadOwner {
        let lease = Lease {
            owner_id: owner_id.into(),
            generation: 1,
            expires_at: 1,
        };
        let mut owner = ThreadOwner::new("thread".into(), lease);
        owner.state = state;
        owner
    }

    fn set(threads: &[&str]) -> BTreeSet<String> {
        threads.iter().map(|thread| (*thread).to_owned()).collect()
    }

    fn work_reading(
        read: impl Fn(&str) -> Result<ThreadOwner, RegistryError> + Send + Sync + 'static,
    ) -> ExternalWork {
        let mut work = ExternalWork::default().with_recheck(Duration::MAX);
        work.set_source(Source {
            owner_id: ME.into(),
            read: Arc::new(read),
        });
        work
    }

    fn settled(work: &mut ExternalWork, candidates: &BTreeSet<String>) {
        work.refresh(candidates, None);
        work.wait_idle();
    }

    #[test]
    fn this_process_holding_the_thread_is_not_another_process_in_any_state() {
        // Given: this process holds the thread, as running or while quiescing
        for state in [
            OwnerState::Running,
            OwnerState::Quiescing,
            OwnerState::Released,
        ] {
            let mut work = work_reading(move |_| Ok(record(ME, state)));

            // When: the thread is a candidate and the probe answered
            settled(&mut work, &set(&["thread"]));

            // Then: it is ours, whether or not it would accept a new turn
            assert!(!work.not_owned_here("thread"), "{state:?}");
        }
    }

    #[test]
    fn another_holder_or_nobody_leaves_the_thread_to_another_process() {
        let mut work = work_reading(|thread| match thread {
            "theirs" => Ok(record("owner-them", OwnerState::Running)),
            _ => Err(RegistryError::Absent),
        });

        settled(&mut work, &set(&["theirs", "nobody"]));

        assert!(work.not_owned_here("theirs"));
        assert!(work.not_owned_here("nobody"));
    }

    #[test]
    fn a_failed_or_missing_probe_is_not_evidence() {
        // Given: reading the registry fails
        let mut work = work_reading(|_| {
            Err(RegistryError::Ownership(
                runtime::ownership::OwnershipError::Fenced,
            ))
        });

        // When: the candidate is probed
        settled(&mut work, &set(&["thread"]));

        // Then: the thread is not claimed to be another process's
        assert!(!work.not_owned_here("thread"));

        // And: before any answer arrives it is unmarked as well
        let mut work = work_reading(|_| Err(RegistryError::Absent));
        work.refresh(&set(&["thread"]), None);
        assert!(!work.not_owned_here("thread"));
        work.wait_idle();
        assert!(work.not_owned_here("thread"));
    }

    #[test]
    fn drawing_again_and_again_probes_each_candidate_once() {
        // Given: a probe that counts its reads
        let reads = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&reads);
        let mut work = work_reading(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            Err(RegistryError::Absent)
        });
        let candidates = set(&["a", "b", "c"]);
        settled(&mut work, &candidates);
        assert_eq!(reads.load(Ordering::SeqCst), 3);

        // When: many more frames ask for the same candidates
        for _ in 0..200 {
            work.refresh(&candidates, None);
            assert!(work.not_owned_here("a"));
        }
        work.wait_idle();

        // Then: no frame read the registry again
        assert_eq!(reads.load(Ordering::SeqCst), 3);

        // And: a changed candidate set or an ownership event probes once more
        settled(&mut work, &set(&["a", "b", "c", "d"]));
        assert_eq!(reads.load(Ordering::SeqCst), 7);
        work.invalidate();
        settled(&mut work, &set(&["a", "b", "c", "d"]));
        assert_eq!(reads.load(Ordering::SeqCst), 11);
    }

    #[test]
    fn a_thread_that_stops_being_a_candidate_forgets_its_answer() {
        let mut work = work_reading(|_| Err(RegistryError::Absent));
        settled(&mut work, &set(&["thread"]));
        assert!(work.not_owned_here("thread"));

        work.refresh(&set(&[]), None);

        assert!(!work.not_owned_here("thread"));
    }

    #[test]
    fn taking_the_thread_unmarks_it_at_once_without_waiting_for_a_probe() {
        // Given: the thread was probed and is another process's
        let held = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&held);
        let mut work = work_reading(move |_| {
            if flag.load(Ordering::SeqCst) {
                Ok(record(ME, OwnerState::Running))
            } else {
                Err(RegistryError::Absent)
            }
        });
        let candidates = set(&["thread"]);
        settled(&mut work, &candidates);
        assert!(work.not_owned_here("thread"));

        // When: this process takes the thread and learns of it
        held.store(true, Ordering::SeqCst);
        work.invalidate();

        // Then: the old verdict is gone before any new probe has answered
        assert!(!work.not_owned_here("thread"));

        // And: the probe that follows confirms the thread as its own
        settled(&mut work, &candidates);
        assert!(!work.not_owned_here("thread"));
    }

    #[test]
    fn an_answer_asked_before_the_thread_was_taken_does_not_mark_it_afterwards() {
        // Given: a probe that reads the registry, then waits to be released;
        // the test is told when each probe has read
        let (read_tx, read_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = std::sync::Mutex::new(release_rx);
        let held = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&held);
        let read_tx = std::sync::Mutex::new(read_tx);
        let mut work = work_reading(move |_| {
            let answer = if flag.load(Ordering::SeqCst) {
                Ok(record(ME, OwnerState::Running))
            } else {
                Err(RegistryError::Absent)
            };
            read_tx
                .lock()
                .expect("read signal")
                .send(())
                .expect("test alive");
            release_rx
                .lock()
                .expect("release")
                .recv()
                .expect("test alive");
            answer
        });
        let candidates = set(&["thread"]);

        // When: a probe has read "nobody holds it" but not yet answered
        work.refresh(&candidates, None);
        read_rx.recv().expect("the probe read");

        // And: this process takes the thread, then the old probe answers
        held.store(true, Ordering::SeqCst);
        work.invalidate();
        release_tx.send(()).expect("release the stale probe");
        work.wait_idle();

        // Then: the stale answer is dropped, not shown as another process's
        assert!(!work.not_owned_here("thread"));

        // And: the probe asked after the change decides, and says it is ours
        work.refresh(&candidates, None);
        read_rx.recv().expect("the second probe read");
        release_tx.send(()).expect("release the second probe");
        work.wait_idle();
        assert!(!work.not_owned_here("thread"));
    }
}
