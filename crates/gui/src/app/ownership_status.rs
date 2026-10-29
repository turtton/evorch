//! Display-only ownership observations. Never use this cache to authorize an operation.

use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use runtime::ownership::{OwnerHost, OwnerState, RegistryError, ThreadOwner};

pub(super) const CONTENTION_GRACE: Duration = Duration::from_secs(1);
/// Probe cadence. A change made by another process shows up within this delay.
const PROBE_INTERVAL: Duration = Duration::from_millis(1000);
/// A snapshot is stale once it is older than a probe cycle plus the grace. An
/// unchanged result is collected by the next probe pass rather than by a
/// repaint, so its age is about `PROBE_INTERVAL` whenever it is displayed.
pub(super) const STALE_AFTER: Duration = PROBE_INTERVAL.saturating_add(CONTENTION_GRACE);
/// Re-check cadence while a probe is outstanding or its result is pending pickup.
const RESULT_RECHECK: Duration = Duration::from_millis(250);
const SLOW_PROBE: Duration = Duration::from_millis(200);
const SLOW_PROBE_LOG_INTERVAL: Duration = Duration::from_secs(5);

struct ProbeRequest {
    revision: u64,
    thread: String,
    readonly: bool,
    host: Arc<OwnerHost>,
    repaint: egui::Context,
    /// The display may change on any result (unknown, stale or superseded).
    urgent: bool,
}

/// What the header shows for a probe result. Heartbeat renewals change the
/// lease expiry without changing this, so they must not wake the UI.
#[derive(Debug, PartialEq, Eq)]
enum ProbeOutcome {
    Unowned,
    Owned {
        owner_id: String,
        generation: u64,
        state: OwnerState,
        writable: bool,
    },
    Failure {
        contention: bool,
        detail: String,
    },
}

impl ProbeOutcome {
    fn of(result: &Result<OwnershipSnapshot, RegistryError>) -> Self {
        match result {
            Ok(OwnershipSnapshot::Owned { owner, writable }) => Self::Owned {
                owner_id: owner.lease.owner_id.clone(),
                generation: owner.lease.generation,
                state: owner.state,
                writable: *writable,
            },
            Ok(OwnershipSnapshot::Unowned) | Err(RegistryError::Absent) => Self::Unowned,
            Err(error) => Self::Failure {
                contention: error.is_contention(),
                detail: error.to_string(),
            },
        }
    }
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
                let mut last_sent: Option<ProbeOutcome> = None;
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
                    // Wake the UI only for a result it would display differently.
                    // An unchanged one waits in the channel for the next probe
                    // pass. Request the repaint first: egui paints two passes
                    // for it, so the result is queued before the second one.
                    let outcome = ProbeOutcome::of(&result);
                    if request.urgent || last_sent.as_ref() != Some(&outcome) {
                        request.repaint.request_repaint();
                    }
                    last_sent = Some(outcome);
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
            urgent: self.invalidated || self.snapshot.is_none() || self.stale(now),
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
                    .is_some_and(|last| now.saturating_duration_since(last) >= STALE_AFTER))
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
                let contention = error.is_contention();
                self.failure = Some(ProbeFailure {
                    since: self.failure.as_ref().map_or(now, |failure| failure.since),
                    contention,
                    detail: error.to_string(),
                });
            }
        }
    }

    /// Time until the display needs a pass again without any other event:
    /// the next probe, a snapshot turning stale, or a contention failure
    /// reaching its grace. A pending result is picked up by the probe pass.
    pub fn next_check_in(&self, now: Instant) -> Duration {
        let until = |deadline: Instant| {
            deadline
                .checked_duration_since(now)
                .filter(|d| !d.is_zero())
        };
        let probe = self
            .last_probe
            .and_then(|last| until(last + PROBE_INTERVAL))
            .unwrap_or(RESULT_RECHECK);
        let stale = (self.snapshot.is_some() && !self.invalidated)
            .then_some(self.last_observed)
            .flatten()
            .and_then(|last| until(last + STALE_AFTER));
        let grace = self
            .failure
            .as_ref()
            .filter(|failure| failure.contention)
            .and_then(|failure| until(failure.since + CONTENTION_GRACE));
        [Some(probe), stale, grace]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(probe)
    }

    /// Pretend a probe was sent at `at` and has not answered, so tests can
    /// run the header without starting a worker.
    #[cfg(test)]
    pub(super) fn mark_probe_outstanding(&mut self, at: Instant) {
        self.in_flight = true;
        self.last_probe = Some(at);
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
