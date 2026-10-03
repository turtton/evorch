//! Single-flight refresh scheduling for the visible Diff tab.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::{DiffError, DiffMode, DiffRequest, DiffSource, DiffState, state_from_result};

pub const AUTO_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const ERROR_RETRY_INTERVAL: Duration = Duration::from_secs(5);

type FetchResult = (u64, DiffRequest, Result<String, DiffError>);

#[derive(Debug)]
struct Slot {
    state: DiffState,
    next_refresh: Option<Instant>,
    error: Option<String>,
}

impl Default for Slot {
    fn default() -> Self {
        Self {
            state: DiffState::Idle,
            next_refresh: None,
            error: None,
        }
    }
}

/// Non-blocking, repository-scoped diff snapshots with one fetch in flight.
#[derive(Debug)]
pub struct DiffModel {
    working_tree: Slot,
    branch: Slot,
    repo_root: Option<PathBuf>,
    generation: u64,
    snapshot: bool,
    in_flight: Option<DiffRequest>,
    rx: Receiver<FetchResult>,
    tx: mpsc::Sender<FetchResult>,
    worker: Option<JoinHandle<()>>,
}

impl DiffModel {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            working_tree: Slot::default(),
            branch: Slot::default(),
            repo_root: None,
            generation: 0,
            snapshot: false,
            in_flight: None,
            rx,
            tx,
            worker: None,
        }
    }

    /// Invalidate both scopes before polling results when project/worktree changes.
    pub fn set_repo_root(&mut self, root: Option<PathBuf>) {
        if self.repo_root != root {
            self.repo_root = root;
            self.generation = self.generation.wrapping_add(1);
            self.working_tree = Slot::default();
            self.branch = Slot::default();
            self.snapshot = false;
        }
    }

    /// Explicit restoration/tool snapshots stay visible until a manual fetch.
    pub fn show_snapshot(&mut self, text: String) {
        self.generation = self.generation.wrapping_add(1);
        self.snapshot = true;
        self.working_tree = Slot {
            state: state_from_result(Ok(text)),
            ..Slot::default()
        };
    }

    pub fn state(&self, mode: &DiffMode) -> &DiffState {
        &self.slot(mode).state
    }

    pub fn refresh_error(&self, mode: &DiffMode) -> Option<&str> {
        self.slot(mode).error.as_deref()
    }

    pub const fn is_snapshot(&self) -> bool {
        self.snapshot
    }

    /// Called only when the tab is rendered. Hidden tabs do not launch Git.
    pub fn refresh_if_due(
        &mut self,
        source: Arc<dyn DiffSource>,
        request: DiffRequest,
        now: Instant,
    ) {
        self.set_repo_root(Some(request.repo_root.clone()));
        if self.snapshot
            || self
                .slot(&request.mode)
                .next_refresh
                .is_some_and(|due| now < due)
        {
            return;
        }
        self.start(source, request);
    }

    /// Manual scope/Refresh action bypasses the interval, but never overlaps fetches.
    pub fn request(&mut self, source: Arc<dyn DiffSource>, request: DiffRequest) {
        self.set_repo_root(Some(request.repo_root.clone()));
        self.snapshot = false;
        self.slot_mut(&request.mode).next_refresh = None;
        self.start(source, request);
    }

    fn start(&mut self, source: Arc<dyn DiffSource>, request: DiffRequest) {
        if self.worker.is_some() {
            return;
        }
        let slot = self.slot_mut(&request.mode);
        if matches!(slot.state, DiffState::Idle) {
            slot.state = DiffState::Loading;
        }
        self.in_flight = Some(request.clone());
        let generation = self.generation;
        let tx = self.tx.clone();
        self.worker = Some(thread::spawn(move || {
            let result = source.fetch(&request);
            let _ = tx.send((generation, request, result));
        }));
    }

    pub fn poll(&mut self) {
        self.poll_at(Instant::now());
    }

    fn poll_at(&mut self, now: Instant) {
        if self.worker.as_ref().is_some_and(JoinHandle::is_finished)
            && let Some(worker) = self.worker.take()
        {
            let _ = worker.join();
        }
        while let Ok((generation, request, result)) = self.rx.try_recv() {
            self.in_flight = None;
            if generation != self.generation || Some(&request.repo_root) != self.repo_root.as_ref()
            {
                continue;
            }
            let slot = self.slot_mut(&request.mode);
            slot.next_refresh = Some(
                now + if result.is_err() {
                    ERROR_RETRY_INTERVAL
                } else {
                    AUTO_REFRESH_INTERVAL
                },
            );
            match result {
                Err(error)
                    if matches!(
                        slot.state,
                        DiffState::Ready { .. } | DiffState::Truncated { .. } | DiffState::Empty
                    ) =>
                {
                    slot.error = Some(error.to_string());
                }
                result => {
                    slot.state = state_from_result(result);
                    slot.error = None;
                }
            }
        }
    }

    fn slot(&self, mode: &DiffMode) -> &Slot {
        match mode {
            DiffMode::WorkingTree => &self.working_tree,
            DiffMode::Branch => &self.branch,
        }
    }

    fn slot_mut(&mut self, mode: &DiffMode) -> &mut Slot {
        match mode {
            DiffMode::WorkingTree => &mut self.working_tree,
            DiffMode::Branch => &mut self.branch,
        }
    }
}

impl Default for DiffModel {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[path = "refresh_tests.rs"]
mod tests;
