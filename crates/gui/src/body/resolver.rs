//! Reads deferred bodies on a worker and keeps what it read, bounded.
//!
//! The UI asks [`BodyResolver::view`] for what to draw; that never blocks and
//! never touches storage. A miss queues the body; the worker batches queued
//! bodies, reads them, and wakes the UI. Requests for a body that is already
//! queued, in flight or cached are one request. A frame names the bodies it
//! draws, so requests for bodies that scrolled away before they started are
//! dropped. Transient failures are retried by themselves on a schedule from an
//! injected clock; integrity failures are not, but can be retried by hand.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{JoinHandle, ThreadId};
use std::time::Duration;

use super::backend::{BodyBackend, EventReread};
use super::clock::{Clock, SystemClock};
use super::text::{BodyText, Segment};
use super::types::{BodyError, BodyErrorKind, BodyOrigin, BodyRef, DeferredBody};

/// Automatic retries after the first failed read of a transient error.
pub const AUTO_RETRIES: u8 = 3;
/// Most distinct bodies in one storage call.
pub const BATCH_REFS: usize = 256;
/// Declared bytes of a normal batch; a larger single body goes alone.
pub const BATCH_BYTES: u64 = 16 * 1024 * 1024;
/// Resolved text kept for reuse.
pub const CACHE_BYTES: usize = 64 * 1024 * 1024;
/// The largest body storage reads at all.
pub const MAX_BODY_BYTES: u64 = 1_000_000_000;
const MAX_ORIGINS: usize = 4;

#[derive(Clone)]
pub struct ResolverConfig {
    pub clock: Arc<dyn Clock>,
    pub cache_bytes: usize,
    pub batch_refs: usize,
    pub batch_bytes: u64,
    pub max_body_bytes: u64,
    pub retry_delays: [Duration; AUTO_RETRIES as usize],
}

impl Default for ResolverConfig {
    fn default() -> Self {
        Self {
            clock: Arc::new(SystemClock::new()),
            cache_bytes: CACHE_BYTES,
            batch_refs: BATCH_REFS,
            batch_bytes: BATCH_BYTES,
            max_body_bytes: MAX_BODY_BYTES,
            retry_delays: [
                Duration::from_millis(250),
                Duration::from_secs(1),
                Duration::from_secs(4),
            ],
        }
    }
}

/// What to draw for a body.
#[derive(Debug, Clone)]
pub enum BodyView {
    Ready(Arc<str>),
    /// Waiting for the read (queued or running).
    Loading {
        bytes: u64,
    },
    /// The last read failed transiently and another try is scheduled.
    Retrying {
        bytes: u64,
        /// Which automatic retry comes next, from 1.
        attempt: u8,
        of: u8,
        retry_in: Duration,
        error: BodyError,
    },
    /// No automatic try is left, or the data is wrong.
    Failed(BodyFailure),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyFailure {
    pub key: BodyRef,
    pub error: BodyError,
    pub origin: BodyOrigin,
    /// Reads attempted since the last manual retry, including the first.
    pub attempts: u32,
}

impl BodyFailure {
    /// What the "copy details" action puts on the clipboard. It names the body
    /// and where it was found; it never contains the text.
    #[must_use]
    pub fn details(&self) -> String {
        let hash = self.key.hash_hex();
        format!(
            "Body could not be read\nerror: {}\nhash: {hash}\nbyte_len: {}\nrun: {}\nevent: {}\nattempts: {}\ninspect: evorch inspect blob {hash}\n",
            self.error,
            self.key.byte_len,
            self.origin.run_id.as_deref().unwrap_or("(none)"),
            self.origin
                .event_id
                .map_or_else(|| "(none)".to_owned(), |id| id.to_string()),
            self.attempts,
        )
    }
}

/// A permanent failure, reported once for the diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureReport {
    pub failure: BodyFailure,
}

/// Counters for tests and the restore census. Time never decides anything.
#[derive(Debug, Clone, Default)]
pub struct ResolverStats {
    pub storage_calls: u64,
    pub bodies_read: u64,
    pub bytes_read: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    /// Views of a body that was already queued, in flight or waiting to retry.
    pub joined_requests: u64,
    pub auto_retries: u64,
    pub manual_retries: u64,
    pub cancelled: u64,
    pub evictions: u64,
    pub late_results_dropped: u64,
    pub largest_batch_refs: usize,
    pub largest_batch_bytes: u64,
    pub event_rereads: u64,
    /// Bodies read at once for the start-up replay (not for a frame).
    pub immediate_reads: u64,
    pub immediate_bytes: u64,
    /// The threads that called the backend; never the UI's.
    pub reader_threads: Vec<ThreadId>,
}

/// How a re-read of an unreadable event is going.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventPhase {
    Idle,
    Loading,
    Failed(String),
}

#[derive(Debug)]
enum Phase {
    Idle,
    Queued {
        seq: u64,
    },
    InFlight {
        request: u64,
    },
    RetryWait {
        retries: u8,
        due: Duration,
        error: BodyError,
    },
    Failed {
        error: BodyError,
        attempts: u32,
    },
    Ready {
        text: Arc<str>,
        oversize: bool,
    },
}

struct Entry {
    phase: Phase,
    origins: Vec<Arc<BodyOrigin>>,
    last_used: u64,
    /// Reads started since the last manual retry or success.
    attempts: u32,
}

impl Entry {
    fn new() -> Self {
        Self {
            phase: Phase::Idle,
            origins: Vec::new(),
            last_used: 0,
            attempts: 0,
        }
    }

    fn note_origin(&mut self, origin: &Arc<BodyOrigin>) {
        if self.origins.iter().any(|known| **known == **origin) {
            return;
        }
        if self.origins.len() < MAX_ORIGINS {
            self.origins.push(Arc::clone(origin));
        }
    }

    fn origin(&self) -> BodyOrigin {
        self.origins
            .first()
            .map(|o| (**o).clone())
            .unwrap_or_default()
    }
}

#[derive(Default)]
struct State {
    entries: HashMap<BodyRef, Entry>,
    wanted: HashSet<BodyRef>,
    wanted_before: HashSet<BodyRef>,
    pins: HashMap<BodyRef, usize>,
    ready_bytes: usize,
    tick: u64,
    seq: u64,
    request: u64,
    reports: Vec<FailureReport>,
    reported: HashSet<(BodyRef, BodyErrorKind)>,
    event_queue: VecDeque<i64>,
    event_reading: Option<i64>,
    event_failed: HashMap<i64, String>,
    event_results: Vec<(i64, EventReread)>,
    stats: ResolverStats,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    backend: Arc<dyn BodyBackend>,
    config: ResolverConfig,
    repaint: Option<Arc<dyn Fn() + Send + Sync>>,
    shutdown: AtomicBool,
}

struct WorkerGuard {
    shared: Arc<Shared>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for WorkerGuard {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
        // Hold the lock while notifying so a worker between its check and its
        // wait cannot miss the wake-up.
        drop(self.shared.state.lock().unwrap_or_else(|e| e.into_inner()));
        self.shared.wake.notify_all();
        if let Some(thread) = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take()
            && thread.thread().id() != std::thread::current().id()
        {
            let _ = thread.join();
        }
    }
}

/// A cheap handle: clones share one resolver.
#[derive(Clone)]
pub struct BodyResolver {
    shared: Arc<Shared>,
    _guard: Arc<WorkerGuard>,
}

enum Job {
    Bodies(Vec<(BodyRef, u64)>),
    Event(i64),
}

impl BodyResolver {
    /// A resolver with its own worker thread. `repaint` is called (from the
    /// worker) whenever something a frame shows may have changed.
    #[must_use]
    pub fn spawn(
        backend: Arc<dyn BodyBackend>,
        config: ResolverConfig,
        repaint: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Self {
        let resolver = Self::build(backend, config, repaint);
        let shared = Arc::clone(&resolver.shared);
        let spawned = std::thread::Builder::new()
            .name("body-resolver".into())
            .spawn(move || shared.worker_loop());
        match spawned {
            Ok(handle) => {
                *resolver
                    ._guard
                    .thread
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(handle);
            }
            Err(error) => tracing::warn!(%error, "the body resolver thread did not start"),
        }
        resolver
    }

    /// A resolver without a thread: requests wait until [`Self::pump`] runs
    /// them, on the caller's thread. For tests, which then decide when each
    /// read completes.
    #[must_use]
    pub fn manual(backend: Arc<dyn BodyBackend>, config: ResolverConfig) -> Self {
        Self::build(backend, config, None)
    }

    fn build(
        backend: Arc<dyn BodyBackend>,
        config: ResolverConfig,
        repaint: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
            backend,
            config,
            repaint,
            shutdown: AtomicBool::new(false),
        });
        Self {
            _guard: Arc::new(WorkerGuard {
                shared: Arc::clone(&shared),
                thread: Mutex::new(None),
            }),
            shared,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.shared.lock()
    }

    /// What to draw for `body` now. Queues the read when nothing has asked for
    /// it, and marks it as shown by this frame.
    #[must_use]
    pub fn view(&self, body: &DeferredBody) -> BodyView {
        let now = self.shared.config.clock.now();
        let (view, queued) = {
            let mut state = self.lock();
            state.view(body, now)
        };
        if queued {
            self.shared.wake.notify_one();
        }
        view
    }

    /// The text of `body` if it is already held, without asking for it. For
    /// things that only decorate a card (a header summary) and are not worth a
    /// read of their own.
    #[must_use]
    pub fn peek(&self, body: &DeferredBody) -> Option<Arc<str>> {
        let mut state = self.lock();
        state.tick += 1;
        let tick = state.tick;
        let entry = state.entries.get_mut(&body.key)?;
        match &entry.phase {
            Phase::Ready { text, .. } => {
                entry.last_used = tick;
                Some(Arc::clone(text))
            }
            _ => None,
        }
    }

    /// Ends a frame: reads that were queued for bodies this frame did not draw
    /// (and no operation waits for) are dropped, and oversized text nobody
    /// shows is released.
    pub fn finish_frame(&self) {
        let mut state = self.lock();
        state.finish_frame();
    }

    /// Tries a failed (or waiting-to-retry) body again now, from a fresh retry
    /// count. A body that is already queued or running is left alone.
    pub fn retry(&self, body: &DeferredBody) {
        let mut state = self.lock();
        let State {
            entries,
            stats,
            seq,
            ..
        } = &mut *state;
        let Some(entry) = entries.get_mut(&body.key) else {
            return;
        };
        if matches!(entry.phase, Phase::Failed { .. } | Phase::RetryWait { .. }) {
            entry.phase = Phase::Queued { seq: *seq };
            *seq += 1;
            entry.attempts = 0;
            stats.manual_retries += 1;
            drop(state);
            self.shared.wake.notify_one();
        }
    }

    /// Holds the bodies of `text` for an operation (copy, a preview) that has
    /// to wait for all of them. Dropping the pin releases them.
    #[must_use]
    pub fn pin(&self, text: &BodyText) -> BodyPin {
        BodyPin {
            resolver: self.clone(),
            text: text.clone(),
            captured: HashMap::new(),
            started: false,
        }
    }

    /// Asks for an unreadable event to be read again.
    pub fn request_event(&self, id: i64) {
        {
            let mut state = self.lock();
            if state.event_queue.contains(&id) || state.event_reading == Some(id) {
                return;
            }
            state.event_failed.remove(&id);
            state.event_queue.push_back(id);
        }
        self.shared.wake.notify_one();
    }

    #[must_use]
    pub fn event_phase(&self, id: i64) -> EventPhase {
        let state = self.lock();
        if state.event_queue.contains(&id) || state.event_reading == Some(id) {
            EventPhase::Loading
        } else if let Some(reason) = state.event_failed.get(&id) {
            EventPhase::Failed(reason.clone())
        } else {
            EventPhase::Idle
        }
    }

    /// The finished event re-reads, for the app to apply to its transcripts.
    #[must_use]
    pub fn take_event_results(&self) -> Vec<(i64, EventReread)> {
        std::mem::take(&mut self.lock().event_results)
    }

    /// Permanent failures not reported yet. One report per body and kind of
    /// failure, until the body is read.
    #[must_use]
    pub fn take_failure_reports(&self) -> Vec<FailureReport> {
        std::mem::take(&mut self.lock().reports)
    }

    /// Forgets everything held for a run: cached text, failures, waiting
    /// reads. A read that finishes afterwards is dropped. Used when a thread
    /// is deleted.
    pub fn forget_run(&self, run_id: &str) {
        let mut state = self.lock();
        let State {
            entries,
            ready_bytes,
            stats,
            ..
        } = &mut *state;
        entries.retain(|_, entry| {
            entry
                .origins
                .retain(|origin| origin.run_id.as_deref() != Some(run_id));
            let keep = !entry.origins.is_empty();
            if !keep {
                if let Phase::Ready {
                    text,
                    oversize: false,
                } = &entry.phase
                {
                    *ready_bytes -= text.len();
                }
                if matches!(entry.phase, Phase::InFlight { .. }) {
                    stats.late_results_dropped += 1;
                }
            }
            keep
        });
    }

    /// Reads every deferred piece of `text` now, on the calling thread, and
    /// joins them. For the start-up replay, which has no frame to wait for; a
    /// UI frame never calls this.
    ///
    /// # Errors
    /// The first body that could not be read.
    pub fn read_now(&self, text: &BodyText) -> Result<String, BodyError> {
        let mut wanted: Vec<BodyRef> = Vec::new();
        for body in text.bodies() {
            if !wanted.contains(&body.key) {
                wanted.push(body.key);
            }
        }
        let mut read: HashMap<BodyRef, Arc<str>> = HashMap::new();
        for chunk in wanted.chunks(self.shared.config.batch_refs.max(1)) {
            let results = self.shared.backend.resolve(chunk)?;
            for (key, result) in chunk.iter().zip(results) {
                let body = result?;
                if body.len() as u64 != key.byte_len {
                    return Err(BodyError::new(
                        BodyErrorKind::Corrupt,
                        "the body's length differs from the one its reference declares",
                    ));
                }
                read.insert(*key, body);
            }
        }
        let mut state = self.lock();
        state.stats.immediate_reads += read.len() as u64;
        state.stats.immediate_bytes += read.values().map(|body| body.len() as u64).sum::<u64>();
        drop(state);
        let mut out = String::with_capacity(text.len());
        for segment in text.segments() {
            match segment {
                Segment::Inline(inline) => out.push_str(inline),
                Segment::Deferred(body) => match read.get(&body.key) {
                    Some(part) => out.push_str(part),
                    None => {
                        return Err(BodyError::new(
                            BodyErrorKind::Missing,
                            "the store answered short",
                        ));
                    }
                },
            }
        }
        Ok(out)
    }

    /// Whether the resolver has no unfinished read or waiting retry.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        let state = self.lock();
        state.event_queue.is_empty()
            && state.event_reading.is_none()
            && state.entries.values().all(|entry| {
                !matches!(
                    entry.phase,
                    Phase::Queued { .. } | Phase::InFlight { .. } | Phase::RetryWait { .. }
                )
            })
    }

    /// The earliest time a scheduled retry is due, if any.
    #[must_use]
    pub fn next_retry_in(&self) -> Option<Duration> {
        let now = self.shared.config.clock.now();
        self.lock().next_due(now)
    }

    #[must_use]
    pub fn stats(&self) -> ResolverStats {
        self.lock().stats.clone()
    }

    /// Bytes of resolved text held for reuse (not counting oversized text).
    #[must_use]
    pub fn cached_bytes(&self) -> usize {
        self.lock().ready_bytes
    }

    /// Runs the pending work on the calling thread: promotes due retries,
    /// reads one batch (or one event). Returns whether it read anything.
    /// For [`Self::manual`] resolvers.
    pub fn pump(&self) -> bool {
        let job = {
            let mut state = self.lock();
            self.shared.take_job(&mut state)
        };
        match job {
            Some(job) => {
                self.shared.execute(job);
                true
            }
            None => false,
        }
    }

    /// Pumps until nothing is runnable now (retries not yet due wait).
    pub fn pump_all(&self) {
        while self.pump() {}
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn worker_loop(&self) {
        let mut state = self.lock();
        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                return;
            }
            if let Some(job) = self.take_job(&mut state) {
                drop(state);
                self.execute(job);
                state = self.lock();
                continue;
            }
            let now = self.config.clock.now();
            state = match state.next_due(now) {
                Some(wait) => {
                    self.wake
                        .wait_timeout(state, wait)
                        .unwrap_or_else(|e| e.into_inner())
                        .0
                }
                None => self.wake.wait(state).unwrap_or_else(|e| e.into_inner()),
            };
        }
    }

    fn take_job(&self, state: &mut State) -> Option<Job> {
        let now = self.config.clock.now();
        state.promote_due(now);
        if let Some(items) = state.take_batch(&self.config) {
            return Some(Job::Bodies(items));
        }
        let id = state.event_queue.pop_front()?;
        state.event_reading = Some(id);
        Some(Job::Event(id))
    }

    fn execute(&self, job: Job) {
        match job {
            Job::Bodies(items) => self.read_bodies(&items),
            Job::Event(id) => self.read_event(id),
        }
        if let Some(repaint) = &self.repaint {
            repaint();
        }
    }

    fn read_bodies(&self, items: &[(BodyRef, u64)]) {
        let refs: Vec<BodyRef> = items.iter().map(|(key, _)| *key).collect();
        let outcome = self.backend.resolve(&refs);
        let now = self.config.clock.now();
        let mut state = self.lock();
        state.stats.storage_calls += 1;
        let thread = std::thread::current().id();
        if !state.stats.reader_threads.contains(&thread) {
            state.stats.reader_threads.push(thread);
        }
        for (index, (key, request)) in items.iter().enumerate() {
            let result = match &outcome {
                Ok(results) => results.get(index).cloned().unwrap_or_else(|| {
                    Err(BodyError::new(
                        BodyErrorKind::Io,
                        "the store answered short",
                    ))
                }),
                Err(error) => Err(error.clone()),
            };
            state.finish_read(key, *request, result, now, &self.config);
        }
    }

    fn read_event(&self, id: i64) {
        let outcome = self.backend.read_event(id);
        let thread = std::thread::current().id();
        let mut state = self.lock();
        state.stats.event_rereads += 1;
        if !state.stats.reader_threads.contains(&thread) {
            state.stats.reader_threads.push(thread);
        }
        state.event_reading = None;
        if let EventReread::Failed(failure) = &outcome {
            state.event_failed.insert(id, failure.reason.clone());
        }
        state.event_results.push((id, outcome));
    }
}

impl State {
    fn priority(&self, key: &BodyRef) -> u8 {
        if self.pins.contains_key(key) {
            0
        } else if self.wanted.contains(key) || self.wanted_before.contains(key) {
            1
        } else {
            2
        }
    }

    fn wanted_or_pinned(&self, key: &BodyRef) -> bool {
        self.pins.contains_key(key) || self.wanted.contains(key) || self.wanted_before.contains(key)
    }

    fn view(&mut self, body: &DeferredBody, now: Duration) -> (BodyView, bool) {
        let key = body.key;
        self.wanted.insert(key);
        self.tick += 1;
        let tick = self.tick;
        let State {
            entries,
            stats,
            seq,
            ..
        } = self;
        let entry = entries.entry(key).or_insert_with(Entry::new);
        entry.note_origin(&body.origin);
        entry.last_used = tick;
        let mut queued = false;
        let view = match &entry.phase {
            Phase::Ready { text, .. } => {
                stats.cache_hits += 1;
                BodyView::Ready(Arc::clone(text))
            }
            Phase::Idle => {
                stats.cache_misses += 1;
                entry.phase = Phase::Queued { seq: *seq };
                *seq += 1;
                queued = true;
                BodyView::Loading {
                    bytes: key.byte_len,
                }
            }
            Phase::Queued { .. } | Phase::InFlight { .. } => {
                stats.joined_requests += 1;
                BodyView::Loading {
                    bytes: key.byte_len,
                }
            }
            Phase::RetryWait {
                retries,
                due,
                error,
            } => {
                stats.joined_requests += 1;
                BodyView::Retrying {
                    bytes: key.byte_len,
                    attempt: retries + 1,
                    of: AUTO_RETRIES,
                    retry_in: due.saturating_sub(now),
                    error: error.clone(),
                }
            }
            Phase::Failed { error, attempts } => BodyView::Failed(BodyFailure {
                key,
                error: error.clone(),
                origin: entry.origin(),
                attempts: *attempts,
            }),
        };
        (view, queued)
    }

    fn finish_frame(&mut self) {
        self.wanted_before = std::mem::take(&mut self.wanted);
        let State {
            entries,
            wanted_before,
            pins,
            stats,
            ..
        } = self;
        for (key, entry) in entries.iter_mut() {
            if wanted_before.contains(key) || pins.contains_key(key) {
                continue;
            }
            match &entry.phase {
                Phase::Queued { .. } | Phase::RetryWait { .. } => {
                    entry.phase = Phase::Idle;
                    entry.attempts = 0;
                    stats.cancelled += 1;
                }
                Phase::Ready { oversize: true, .. } => entry.phase = Phase::Idle,
                _ => {}
            }
        }
    }

    fn promote_due(&mut self, now: Duration) {
        let due: Vec<BodyRef> = self
            .entries
            .iter()
            .filter(|(key, entry)| {
                matches!(entry.phase, Phase::RetryWait { due, .. } if due <= now)
                    && self.wanted_or_pinned(key)
            })
            .map(|(key, _)| *key)
            .collect();
        for key in due {
            let seq = self.seq;
            self.seq += 1;
            if let Some(entry) = self.entries.get_mut(&key) {
                entry.phase = Phase::Queued { seq };
                self.stats.auto_retries += 1;
            }
        }
    }

    fn next_due(&self, now: Duration) -> Option<Duration> {
        self.entries
            .iter()
            .filter_map(|(key, entry)| match entry.phase {
                Phase::RetryWait { due, .. } if self.wanted_or_pinned(key) => {
                    Some(due.saturating_sub(now))
                }
                _ => None,
            })
            .min()
    }

    fn take_batch(&mut self, config: &ResolverConfig) -> Option<Vec<(BodyRef, u64)>> {
        let mut queued: Vec<(u8, u64, BodyRef)> = self
            .entries
            .iter()
            .filter_map(|(key, entry)| match entry.phase {
                Phase::Queued { seq } => Some((self.priority(key), seq, *key)),
                _ => None,
            })
            .collect();
        if queued.is_empty() {
            return None;
        }
        queued.sort_unstable();
        let mut batch: Vec<BodyRef> = Vec::new();
        let mut bytes = 0u64;
        for (_, _, key) in queued {
            if key.byte_len > config.max_body_bytes {
                self.fail(
                    &key,
                    BodyError::new(
                        BodyErrorKind::TooLarge,
                        format!(
                            "the body is {} bytes, over the {} byte limit",
                            key.byte_len, config.max_body_bytes
                        ),
                    ),
                );
                continue;
            }
            if batch.is_empty() {
                batch.push(key);
                bytes = key.byte_len;
                if bytes >= config.batch_bytes {
                    break;
                }
                continue;
            }
            if batch.len() >= config.batch_refs {
                break;
            }
            if bytes.saturating_add(key.byte_len) > config.batch_bytes {
                continue;
            }
            batch.push(key);
            bytes += key.byte_len;
        }
        if batch.is_empty() {
            return None;
        }
        self.stats.largest_batch_refs = self.stats.largest_batch_refs.max(batch.len());
        self.stats.largest_batch_bytes = self.stats.largest_batch_bytes.max(bytes);
        let mut items = Vec::with_capacity(batch.len());
        for key in batch {
            self.request += 1;
            let request = self.request;
            if let Some(entry) = self.entries.get_mut(&key) {
                entry.phase = Phase::InFlight { request };
                entry.attempts += 1;
            }
            items.push((key, request));
        }
        Some(items)
    }

    fn fail(&mut self, key: &BodyRef, error: BodyError) {
        let Some(entry) = self.entries.get_mut(key) else {
            return;
        };
        let attempts = entry.attempts.max(1);
        entry.phase = Phase::Failed {
            error: error.clone(),
            attempts,
        };
        let failure = BodyFailure {
            key: *key,
            error: error.clone(),
            origin: entry.origin(),
            attempts,
        };
        if self.reported.insert((*key, error.kind)) {
            self.reports.push(FailureReport { failure });
        }
    }

    fn finish_read(
        &mut self,
        key: &BodyRef,
        request: u64,
        result: Result<Arc<str>, BodyError>,
        now: Duration,
        config: &ResolverConfig,
    ) {
        let current = matches!(
            self.entries.get(key),
            Some(Entry { phase: Phase::InFlight { request: r }, .. }) if *r == request
        );
        if !current {
            self.stats.late_results_dropped += 1;
            return;
        }
        match result {
            Ok(text) if text.len() as u64 == key.byte_len => {
                self.stats.bodies_read += 1;
                self.stats.bytes_read += key.byte_len;
                self.store_ready(key, text, config);
            }
            Ok(_) => self.fail(
                key,
                BodyError::new(
                    BodyErrorKind::Corrupt,
                    "the body's length differs from the one its reference declares",
                ),
            ),
            Err(error) if error.kind.is_transient() => {
                let retries = self
                    .entries
                    .get(key)
                    .map_or(0, |entry| entry.attempts.saturating_sub(1));
                let retries = u8::try_from(retries).unwrap_or(u8::MAX);
                if retries < AUTO_RETRIES {
                    let due = now + config.retry_delays[usize::from(retries)];
                    if let Some(entry) = self.entries.get_mut(key) {
                        entry.phase = Phase::RetryWait {
                            retries,
                            due,
                            error,
                        };
                    }
                } else {
                    self.fail(key, error);
                }
            }
            Err(error) => self.fail(key, error),
        }
    }

    fn store_ready(&mut self, key: &BodyRef, text: Arc<str>, config: &ResolverConfig) {
        self.reported.retain(|(reported, _)| reported != key);
        self.tick += 1;
        let tick = self.tick;
        let oversize = text.len() > config.cache_bytes;
        let len = text.len();
        if let Some(entry) = self.entries.get_mut(key) {
            entry.phase = Phase::Ready { text, oversize };
            entry.attempts = 0;
            entry.last_used = tick;
        }
        if !oversize {
            self.ready_bytes += len;
            self.evict_over(config.cache_bytes, key);
        }
    }

    /// Drops the least recently used resolved text until the cache fits.
    fn evict_over(&mut self, cap: usize, keep: &BodyRef) {
        while self.ready_bytes > cap {
            let victim = self
                .entries
                .iter()
                .filter(|(key, entry)| {
                    *key != keep
                        && matches!(
                            entry.phase,
                            Phase::Ready {
                                oversize: false,
                                ..
                            }
                        )
                })
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| *key);
            let Some(victim) = victim else { break };
            if let Some(entry) = self.entries.get_mut(&victim)
                && let Phase::Ready { text, .. } = &entry.phase
            {
                self.ready_bytes -= text.len();
                entry.phase = Phase::Idle;
                self.stats.evictions += 1;
            }
        }
    }
}

/// A set of bodies held for an operation that needs all their text.
pub struct BodyPin {
    resolver: BodyResolver,
    text: BodyText,
    captured: HashMap<BodyRef, Arc<str>>,
    started: bool,
}

/// How an operation waiting on a [`BodyPin`] stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinState {
    /// Still reading; `bytes` is what is declared in total.
    Pending {
        bytes: u64,
    },
    Ready(String),
    Failed(BodyFailure),
}

impl BodyPin {
    /// Asks for what is missing and reports where it stands. Call each frame
    /// until it is ready or failed.
    pub fn poll(&mut self) -> PinState {
        if !self.started {
            self.started = true;
            let mut state = self.resolver.lock();
            for body in self.text.bodies() {
                *state.pins.entry(body.key).or_insert(0) += 1;
            }
        }
        let mut pending = false;
        let mut notify = false;
        for body in self.text.bodies() {
            if self.captured.contains_key(&body.key) {
                continue;
            }
            let mut state = self.resolver.lock();
            let State {
                entries,
                stats,
                seq,
                ..
            } = &mut *state;
            let entry = entries.entry(body.key).or_insert_with(Entry::new);
            entry.note_origin(&body.origin);
            match &entry.phase {
                Phase::Ready { text, .. } => {
                    self.captured.insert(body.key, Arc::clone(text));
                }
                Phase::Idle => {
                    stats.cache_misses += 1;
                    entry.phase = Phase::Queued { seq: *seq };
                    *seq += 1;
                    notify = true;
                    pending = true;
                }
                Phase::Failed { error, attempts } => {
                    return PinState::Failed(BodyFailure {
                        key: body.key,
                        error: error.clone(),
                        origin: entry.origin(),
                        attempts: *attempts,
                    });
                }
                Phase::Queued { .. } | Phase::InFlight { .. } | Phase::RetryWait { .. } => {
                    pending = true;
                }
            }
        }
        if notify {
            self.resolver.shared.wake.notify_one();
        }
        if pending {
            return PinState::Pending {
                bytes: self.text.byte_len(),
            };
        }
        let mut out = String::with_capacity(self.text.len());
        for segment in self.text.segments() {
            match segment {
                Segment::Inline(text) => out.push_str(text),
                Segment::Deferred(body) => {
                    if let Some(text) = self.captured.get(&body.key) {
                        out.push_str(text);
                    }
                }
            }
        }
        PinState::Ready(out)
    }

    /// Tries the failed bodies of this pin again.
    pub fn retry(&self) {
        for body in self.text.bodies() {
            self.resolver.retry(body);
        }
    }
}

impl Drop for BodyPin {
    fn drop(&mut self) {
        if !self.started {
            return;
        }
        let mut state = self.resolver.lock();
        for body in self.text.bodies() {
            if let Some(count) = state.pins.get_mut(&body.key) {
                *count -= 1;
                if *count == 0 {
                    state.pins.remove(&body.key);
                }
            }
        }
    }
}
