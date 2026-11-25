//! Auto-archive owner and reports. PR1 deliberately never supplies production authority.
use crate::CommandHost;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak, mpsc};
use storage::thread_retention::activity::ActivityReport;
use storage::thread_retention::{AutoArchiveAuthority, AutoArchivePolicy};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RetentionReport {
    pub activity: ActivityReport,
    /// Per-tick counts, not global candidate totals. Reasons contain no message text.
    pub holds: BTreeMap<&'static str, usize>,
    pub inspected_runs: usize,
    pub published: usize,
    pub attachment_calls: usize,
    pub scratch_hints: usize,
    pub failures: Vec<String>,
}
impl RetentionReport {
    pub(crate) fn hold(&mut self, reason: &'static str) {
        *self.holds.entry(reason).or_default() += 1;
    }
    pub(crate) fn failure(&mut self, error: impl std::fmt::Display) {
        if self.failures.len() < 8 {
            self.failures
                .push(error.to_string().chars().take(256).collect());
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Member {
    pub id: workspace_state::ThreadId,
    pub runs: Vec<String>,
    pub created: i64,
}
#[derive(Clone, Debug)]
pub(crate) struct RunProof {
    pub run: String,
    pub revision: i64,
    pub gate: runtime::thread_retention::gate::GateStamp,
}
#[derive(Clone, Debug)]
pub(crate) struct Candidate {
    pub key: workspace_state::ThreadId,
    pub members: Vec<Member>,
    pub archive_revision: u64,
    pub pin_revision: u64,
    pub member: usize,
    pub run: usize,
    pub db_after: i64,
    pub binding_revision: i64,
    pub seen: std::collections::HashSet<String>,
    pub activity: i64,
    pub proofs: Vec<RunProof>,
    pub global_gate: runtime::thread_retention::gate::GateStamp,
    pub thread_gates: BTreeMap<String, runtime::thread_retention::gate::GateStamp>,
}
/// At most one family publication, one remove_thread and one scratch member per tick.
pub struct AutoArchiveCoordinator {
    pub(crate) config: storage::StorageConfig,
    root: PathBuf,
    pub(crate) policy: AutoArchivePolicy,
    pub(crate) after_family: Option<String>,
    pub(crate) cleanup_after: Option<String>,
    pub(crate) candidate: Option<Candidate>,
    stopped: bool,
    last_holds: BTreeMap<&'static str, usize>,
    last_global_failures: (u64, Vec<i64>),
}
impl AutoArchiveCoordinator {
    pub fn new(config: storage::StorageConfig, root: PathBuf, policy: AutoArchivePolicy) -> Self {
        Self {
            config,
            root,
            policy,
            after_family: None,
            cleanup_after: None,
            candidate: None,
            stopped: false,
            last_holds: BTreeMap::new(),
            last_global_failures: Default::default(),
        }
    }
    pub fn stop(&mut self) {
        self.stopped = true;
        self.candidate = None;
    }
    pub fn tick(
        &mut self,
        host: &Arc<Mutex<CommandHost>>,
        authority: Option<&AutoArchiveAuthority<'_>>,
        now_ns: i64,
    ) -> RetentionReport {
        let mut report = RetentionReport::default();
        if self.stopped {
            report.hold("StopRequested");
            return report;
        }
        // This branch precedes host/sidebar/attachment access or candidate expansion.
        let Some(authority) = authority else {
            report.hold("AuthorityUnavailable");
            match storage::Database::open(&self.config).and_then(|db| db.activity_report()) {
                Ok(progress) => report.activity = progress,
                Err(error) => report.failure(error),
            }
            self.log(&report);
            return report;
        };
        if let Err(error) = authority.validate(&self.config.db_path, &self.root) {
            report.hold("AuthorityUnavailable");
            report.failure(error);
            return report;
        }
        let Ok(host) = host.try_lock() else {
            report.hold("ForegroundPending");
            return report;
        };
        if !host
            .areas
            .workspace
            .accepts_auto_archive_authority(authority, &self.root)
        {
            report.hold("AuthorityUnavailable");
            return report;
        }
        let db = match storage::Database::open(&self.config) {
            Ok(db) => db,
            Err(error) => {
                report.failure(error);
                return report;
            }
        };
        if let Ok(progress) = db.activity_report() {
            report.activity = progress
        }
        host.areas
            .workspace
            .auto_archive_tick(self, &db, now_ns, &mut report);
        drop(host);
        self.log(&report);
        report
    }
    fn log(&mut self, report: &RetentionReport) {
        let global_failures = (
            report.activity.global_failures,
            report.activity.global_failure_event_ids.clone(),
        );
        if self.last_holds != report.holds
            || self.last_global_failures != global_failures
            || !report.failures.is_empty()
            || report.published > 0
        {
            tracing::info!(target:"thread_retention",holds=?report.holds,published=report.published,inspected_runs=report.inspected_runs,backfill_pending=report.activity.backfill_pending,pending_sources=report.activity.pending_sources,global_failures=report.activity.global_failures,global_failure_event_ids=?report.activity.global_failure_event_ids,legacy_fallback=report.activity.legacy_fallback,missing_activity_and_fallback=report.activity.missing_activity_and_fallback,attachment_calls=report.attachment_calls,scratch_hints=report.scratch_hints,failures=report.failures.len(),"auto archive tick");
            self.last_holds = report.holds.clone();
            self.last_global_failures = global_failures;
        }
    }
}
/// Dedicated blocking owner. No leases or authority can be installed through it in PR1.
pub struct AutoArchiveOwner {
    producer: runtime::producer::ProducerGate,
    stop: mpsc::Sender<()>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl AutoArchiveOwner {
    pub fn start(
        host: Weak<Mutex<CommandHost>>,
        mut coordinator: AutoArchiveCoordinator,
    ) -> std::io::Result<Self> {
        let (stop, receive) = mpsc::channel();
        let producer = runtime::producer::ProducerGate::default();
        let admission = producer.clone();
        let worker = std::thread::Builder::new()
            .name("auto-archive".into())
            .spawn(move || {
                loop {
                    match receive.recv_timeout(std::time::Duration::from_secs(5)) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    let Some(_permit) = admission.try_enter() else {
                        continue;
                    };
                    let Some(host) = host.upgrade() else { break };
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .ok()
                        .and_then(|d| i64::try_from(d.as_nanos()).ok());
                    if let Some(now) = now {
                        coordinator.tick(&host, None, now);
                    }
                }
                coordinator.stop();
            })?;
        Ok(Self {
            producer,
            stop,
            worker: Some(worker),
        })
    }
    pub fn producer_gate(&self) -> runtime::producer::ProducerGate {
        self.producer.clone()
    }
    pub fn request_stop(&self) {
        self.producer.close();
        let _ = self.stop.send(());
    }
    pub fn shutdown(&mut self) -> Result<(), String> {
        self.request_stop();
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| "auto archive worker panicked".to_owned())?;
        }
        Ok(())
    }
}
impl Drop for AutoArchiveOwner {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}
