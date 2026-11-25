//! Auto-archive owner and reports. PR1 deliberately never supplies production authority.
use crate::CommandHost;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak, mpsc};
use storage::thread_retention::decision::DecisionStats;
use storage::thread_retention::{AutoArchiveAuthority, AutoArchivePolicy};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RetentionReport {
    /// Per-tick counts, not global candidate totals. Reasons contain no message text.
    pub holds: BTreeMap<&'static str, usize>,
    /// What the family decision of this tick read, if one ran.
    pub decision: Option<DecisionStats>,
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
/// The only state kept between ticks is where the next scan starts: every verdict is
/// taken from the canonical sources inside one tick. One family decision, one
/// publication, one `remove_thread` and one scratch member at most per tick.
pub struct AutoArchiveCoordinator {
    pub(crate) config: storage::StorageConfig,
    root: PathBuf,
    pub(crate) policy: AutoArchivePolicy,
    pub(crate) after_family: Option<String>,
    pub(crate) cleanup_after: Option<String>,
    stopped: bool,
    last_holds: BTreeMap<&'static str, usize>,
    #[cfg(feature = "test-support")]
    pub(crate) after_decision: Option<Box<dyn FnMut() + Send>>,
}
impl AutoArchiveCoordinator {
    pub fn new(config: storage::StorageConfig, root: PathBuf, policy: AutoArchivePolicy) -> Self {
        Self {
            config,
            root,
            policy,
            after_family: None,
            cleanup_after: None,
            stopped: false,
            last_holds: BTreeMap::new(),
            #[cfg(feature = "test-support")]
            after_decision: None,
        }
    }
    /// Runs between the family decision and the admission re-check (a test places an
    /// admission exactly there).
    #[cfg(feature = "test-support")]
    pub fn on_decision(&mut self, hook: impl FnMut() + Send + 'static) {
        self.after_decision = Some(Box::new(hook));
    }
    pub fn stop(&mut self) {
        self.stopped = true;
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
        // Nothing but this report precedes the check: no database, host, sidebar or
        // attachment access, so another process holding any of them is never disturbed.
        let Some(authority) = authority else {
            report.hold("AuthorityUnavailable");
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
        host.areas
            .workspace
            .auto_archive_tick(self, &db, now_ns, &mut report);
        drop(host);
        self.log(&report);
        report
    }
    fn log(&mut self, report: &RetentionReport) {
        if self.last_holds != report.holds || !report.failures.is_empty() || report.published > 0 {
            tracing::info!(target:"thread_retention",holds=?report.holds,published=report.published,inspected_runs=report.inspected_runs,decision=?report.decision,attachment_calls=report.attachment_calls,scratch_hints=report.scratch_hints,failures=report.failures.len(),"auto archive tick");
            self.last_holds = report.holds.clone();
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
