//! Display-only ownership observations. Never use this cache to authorize an operation.

use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use runtime::ownership::{OwnerHost, RegistryError, ThreadOwner};

const CONTENTION_GRACE: Duration = Duration::from_secs(1);
const PROBE_INTERVAL: Duration = Duration::from_millis(250);
const SLOW_PROBE: Duration = Duration::from_millis(200);
const SLOW_PROBE_LOG_INTERVAL: Duration = Duration::from_secs(5);

struct ProbeRequest {
    revision: u64,
    thread: String,
    readonly: bool,
    host: Arc<OwnerHost>,
    repaint: egui::Context,
}

struct ProbeResult {
    revision: u64,
    thread: String,
    readonly: bool,
    result: Result<OwnershipSnapshot, RegistryError>,
    observed_at: Instant,
}

#[derive(Debug)]
struct ProbeWorker {
    requests: mpsc::SyncSender<ProbeRequest>,
    results: mpsc::Receiver<ProbeResult>,
}

impl ProbeWorker {
    fn start(
        mut probe: impl FnMut(&OwnerHost, &str, bool) -> Result<OwnershipSnapshot, RegistryError>
        + Send
        + 'static,
    ) -> Result<Self, RegistryError> {
        let (requests, receiver) = mpsc::sync_channel::<ProbeRequest>(1);
        let (sender, results) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("ownership-display".into())
            .spawn(move || {
                let mut last_slow_log: Option<Instant> = None;
                while let Ok(request) = receiver.recv() {
                    let started = Instant::now();
                    let result = probe(&request.host, &request.thread, request.readonly);
                    let observed_at = Instant::now();
                    let elapsed = observed_at.saturating_duration_since(started);
                    if elapsed >= SLOW_PROBE
                        && last_slow_log.is_none_or(|last| {
                            observed_at.saturating_duration_since(last) >= SLOW_PROBE_LOG_INTERVAL
                        })
                    {
                        tracing::warn!(
                            target: "gui::ownership",
                            duration_ms = elapsed.as_secs_f64() * 1000.0,
                            "slow background ownership display probe"
                        );
                        last_slow_log = Some(observed_at);
                    }
                    if sender
                        .send(ProbeResult {
                            revision: request.revision,
                            thread: request.thread,
                            readonly: request.readonly,
                            result,
                            observed_at,
                        })
                        .is_err()
                    {
                        break;
                    }
                    request.repaint.request_repaint();
                }
            })?;
        Ok(Self { requests, results })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum OwnershipSnapshot {
    Unowned,
    Owned { owner: ThreadOwner, writable: bool },
}

#[derive(Debug)]
pub(super) struct ProbeFailure {
    pub since: Instant,
    pub contention: bool,
    pub detail: String,
}

#[derive(Debug, Default)]
pub(super) struct OwnershipStatus {
    pub thread: Option<String>,
    pub snapshot: Option<OwnershipSnapshot>,
    pub failure: Option<ProbeFailure>,
    worker: Option<ProbeWorker>,
    revision: u64,
    in_flight: bool,
    last_probe: Option<Instant>,
    last_observed: Option<Instant>,
    invalidated: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct OwnershipDisplay {
    pub access: &'static str,
    pub warning: bool,
}

impl OwnershipStatus {
    pub fn select_thread(&mut self, thread: Option<&str>) -> bool {
        if self.thread.as_deref() == thread {
            return false;
        }
        self.thread = thread.map(str::to_owned);
        self.snapshot = None;
        self.failure = None;
        self.last_observed = None;
        self.invalidate();
        true
    }

    /// Supersede pending observations after an explicit action and immediately
    /// mark the previous display stale. Keep a single worker during thread switches.
    pub fn invalidate(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        self.last_probe = None;
        self.invalidated = true;
    }

    pub fn refresh(
        &mut self,
        host: Arc<OwnerHost>,
        thread: &str,
        readonly: bool,
        repaint: &egui::Context,
        now: Instant,
    ) {
        if let Some(worker) = &self.worker {
            match worker.results.try_recv() {
                Ok(result) => {
                    self.accept_result(result, thread, readonly);
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.in_flight = false;
                    self.worker = None;
                    self.observe(
                        thread,
                        Err(std::io::Error::other("ownership display worker stopped").into()),
                        now,
                    );
                }
            }
        }
        if self.in_flight
            || self
                .last_probe
                .is_some_and(|last| now.saturating_duration_since(last) < PROBE_INTERVAL)
        {
            return;
        }
        self.last_probe = Some(now);
        if self.worker.is_none() {
            match ProbeWorker::start(super::ownership::probe_ownership) {
                Ok(worker) => self.worker = Some(worker),
                Err(error) => {
                    self.observe(thread, Err(error), now);
                    return;
                }
            }
        }
        let request = ProbeRequest {
            revision: self.revision,
            thread: thread.to_owned(),
            readonly,
            host,
            repaint: repaint.clone(),
        };
        match self.worker.as_ref().unwrap().requests.try_send(request) {
            Ok(()) => self.in_flight = true,
            Err(error) => {
                self.worker = None;
                self.observe(
                    thread,
                    Err(std::io::Error::other(error.to_string()).into()),
                    now,
                );
            }
        }
    }

    fn accept_result(&mut self, result: ProbeResult, thread: &str, readonly: bool) {
        self.in_flight = false;
        if result.revision == self.revision
            && result.thread == thread
            && result.readonly == readonly
        {
            self.observe(thread, result.result, result.observed_at);
        } else {
            // Only the latest selection is queried after completion.
            self.last_probe = None;
        }
    }

    pub fn stale(&self, now: Instant) -> bool {
        self.snapshot.is_some()
            && (self.invalidated
                || self
                    .last_observed
                    .is_some_and(|last| now.saturating_duration_since(last) >= CONTENTION_GRACE))
    }

    pub fn observe(
        &mut self,
        thread: &str,
        result: Result<OwnershipSnapshot, RegistryError>,
        now: Instant,
    ) {
        self.select_thread(Some(thread));
        match result {
            Ok(snapshot) => {
                self.snapshot = Some(snapshot);
                self.failure = None;
                self.last_observed = Some(now);
                self.invalidated = false;
            }
            Err(RegistryError::Absent) => {
                self.snapshot = Some(OwnershipSnapshot::Unowned);
                self.failure = None;
                self.last_observed = Some(now);
                self.invalidated = false;
            }
            Err(error) => {
                let contention = matches!(
                    &error,
                    RegistryError::Sql(rusqlite::Error::SqliteFailure(error, _))
                        if matches!(error.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
                );
                self.failure = Some(ProbeFailure {
                    since: self.failure.as_ref().map_or(now, |failure| failure.since),
                    contention,
                    detail: error.to_string(),
                });
            }
        }
    }

    pub fn display(&self, now: Instant) -> OwnershipDisplay {
        OwnershipDisplay {
            access: match &self.snapshot {
                Some(OwnershipSnapshot::Owned { writable: true, .. }) => "write",
                Some(OwnershipSnapshot::Unowned | OwnershipSnapshot::Owned { .. }) => "read-only",
                None => "checking ownership",
            },
            warning: self.stale(now)
                || self.failure.as_ref().is_some_and(|failure| {
                    !failure.contention
                        || now.saturating_duration_since(failure.since) >= CONTENTION_GRACE
                }),
        }
    }
}

#[cfg(test)]
#[path = "ownership_status_tests.rs"]
mod tests;
