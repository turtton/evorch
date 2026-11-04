//! A scriptable body store for tests: it answers only when a test lets the
//! resolver run, and records every call.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;

use super::backend::{BodyBackend, BodyResult, HistoryRow};
use super::meter::ImmediateMeter;
use super::types::{BodyError, BodyRef, DeferredBody};

type Hook = Box<dyn Fn() + Send + Sync>;

#[derive(Default)]
struct Script {
    bodies: HashMap<BodyRef, String>,
    /// Errors to answer with before the body is served.
    fail_first: HashMap<BodyRef, VecDeque<BodyError>>,
    fail_always: HashMap<BodyRef, BodyError>,
    outer_failures: VecDeque<BodyError>,
    calls: Vec<Vec<BodyRef>>,
    threads: Vec<ThreadId>,
    rows: VecDeque<Result<Vec<HistoryRow>, BodyError>>,
    rows_reads: Vec<(Vec<String>, i64, i64)>,
}

#[derive(Default)]
pub struct FakeBackend {
    script: Mutex<Script>,
    during_read: Mutex<Option<Hook>>,
}

impl FakeBackend {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn script(&self) -> std::sync::MutexGuard<'_, Script> {
        self.script.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Stores `text` and returns the reference that names it.
    pub fn add(&self, text: &str) -> BodyRef {
        let key = Self::key_of(text);
        self.script().bodies.insert(key, text.to_owned());
        key
    }

    /// The reference of `text`, stored or not.
    #[must_use]
    pub fn key_of(text: &str) -> BodyRef {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::hash::DefaultHasher::new();
        text.hash(&mut hasher);
        let seed = hasher.finish().to_le_bytes();
        let mut hash = [0u8; 32];
        for (index, byte) in hash.iter_mut().enumerate() {
            *byte = seed[index % 8].wrapping_add(index as u8);
        }
        BodyRef {
            hash,
            byte_len: text.len() as u64,
        }
    }

    /// A deferred body for `text`, found in `run` by `event`.
    pub fn deferred(&self, text: &str, run: &str, event: i64) -> DeferredBody {
        DeferredBody::new(self.add(text), Some(run), Some(event))
    }

    /// The next `times` reads of `key` fail with `error`; then it is served.
    pub fn fail_first(&self, key: BodyRef, error: &BodyError, times: usize) {
        self.script()
            .fail_first
            .entry(key)
            .or_default()
            .extend(std::iter::repeat_n(error.clone(), times));
    }

    pub fn fail_always(&self, key: BodyRef, error: BodyError) {
        self.script().fail_always.insert(key, error);
    }

    pub fn stop_failing(&self, key: &BodyRef) {
        let mut script = self.script();
        script.fail_always.remove(key);
        script.fail_first.remove(key);
    }

    /// The next whole storage call fails.
    pub fn fail_next_call(&self, error: BodyError) {
        self.script().outer_failures.push_back(error);
    }

    /// What the next read of rows answers, in turn (an empty list when none is
    /// scripted).
    pub fn script_rows(&self, outcome: Result<Vec<HistoryRow>, BodyError>) {
        self.script().rows.push_back(outcome);
    }

    /// Runs `hook` while a read is in progress, before it answers.
    pub fn on_read(&self, hook: impl Fn() + Send + Sync + 'static) {
        *self.during_read.lock().unwrap_or_else(|e| e.into_inner()) = Some(Box::new(hook));
    }

    #[must_use]
    pub fn calls(&self) -> Vec<Vec<BodyRef>> {
        self.script().calls.clone()
    }

    #[must_use]
    pub fn bodies_read(&self) -> usize {
        self.script().calls.iter().map(Vec::len).sum()
    }

    /// The rows reads so far: the runs, `after` and `through`.
    #[must_use]
    pub fn rows_reads(&self) -> Vec<(Vec<String>, i64, i64)> {
        self.script().rows_reads.clone()
    }

    /// The threads that read from the store.
    #[must_use]
    pub fn reader_threads(&self) -> Vec<ThreadId> {
        self.script().threads.clone()
    }
}

impl BodyBackend for FakeBackend {
    fn resolve(&self, refs: &[BodyRef]) -> Result<Vec<BodyResult>, BodyError> {
        if let Some(hook) = &*self.during_read.lock().unwrap_or_else(|e| e.into_inner()) {
            hook();
        }
        let mut script = self.script();
        script.calls.push(refs.to_vec());
        let thread = std::thread::current().id();
        if !script.threads.contains(&thread) {
            script.threads.push(thread);
        }
        if let Some(error) = script.outer_failures.pop_front() {
            return Err(error);
        }
        let mut answers = Vec::with_capacity(refs.len());
        for key in refs {
            if let Some(error) = script.fail_always.get(key) {
                answers.push(Err(error.clone()));
                continue;
            }
            if let Some(error) = script.fail_first.get_mut(key).and_then(VecDeque::pop_front) {
                answers.push(Err(error));
                continue;
            }
            answers.push(match script.bodies.get(key) {
                Some(text) => Ok(Arc::from(text.as_str())),
                None => Err(BodyError::new(
                    super::types::BodyErrorKind::Missing,
                    "no such body",
                )),
            });
        }
        Ok(answers)
    }

    fn read_rows(
        &self,
        runs: &[String],
        after: i64,
        through: i64,
        _meter: &ImmediateMeter,
    ) -> Result<Vec<HistoryRow>, BodyError> {
        if let Some(hook) = &*self.during_read.lock().unwrap_or_else(|e| e.into_inner()) {
            hook();
        }
        let mut script = self.script();
        script.rows_reads.push((runs.to_vec(), after, through));
        let thread = std::thread::current().id();
        if !script.threads.contains(&thread) {
            script.threads.push(thread);
        }
        script.rows.pop_front().unwrap_or_else(|| Ok(Vec::new()))
    }
}
