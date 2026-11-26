//! Core-owned automatic archive, authorized by dedicated DB/root writer leases.
mod online;
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
    pub confirmed: usize,
    pub stopped_runs: usize,
    pub host_lock_ns: u64,
    pub idle_stop_ns: u64,
    pub writer_roundtrip_ns: u64,
    pub seal_ns: u64,
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
    online_policy: Option<storage::thread_retention::ThreadRetentionPolicy>,
    delete_turn: bool,
    delete_after: Option<String>,
    delete_started: bool,
    deleted_attachment_after: Option<String>,
    permanent_holds: BTreeMap<String, &'static str>,
    send_slot: Arc<Mutex<()>>,
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
            online_policy: None,
            delete_turn: false,
            delete_after: None,
            delete_started: false,
            deleted_attachment_after: None,
            permanent_holds: BTreeMap::new(),
            send_slot: Arc::new(Mutex::new(())),
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
        let slot = self.send_slot.clone();
        let _send_permit = slot.lock().unwrap_or_else(|e| e.into_inner());
        let host_arc = host;
        let Ok(host) = host_arc.try_lock() else {
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
        let binding = host.areas.workspace.auto_archive_binding();
        drop(host);
        // Connection setup, filesystem durability and the writer ACK are outside the host mutex.
        let db = match storage::Database::open(&self.config) {
            Ok(db) => db,
            Err(error) => {
                report.failure(error);
                return report;
            }
        };
        let cleanup_bound = binding.as_ref().is_some_and(|(writer, sidebar)| {
            match runtime::thread_retention::binding::ensure_binding(
                &db,
                writer,
                authority,
                runtime::thread_retention::binding::BindingPaths {
                    ownership_root: &self.root,
                    sidebar_path: sidebar,
                },
                &self.config.db_path,
                now_ns,
            ) {
                Ok(runtime::thread_retention::binding::BindingStatus::Bound) => true,
                Ok(status) => {
                    report.failure(format!("root cleanup disabled: {status:?}"));
                    false
                }
                Err(error) => {
                    report.failure(error);
                    false
                }
            }
        });
        if self.online_policy.is_some() {
            let deleting = self.delete_turn;
            self.delete_turn = !self.delete_turn;
            if deleting {
                self.online_tick(host_arc, authority, &db, now_ns, cleanup_bound, &mut report);
                self.log(&report);
                return report;
            }
        }
        let Ok(host) = host_arc.try_lock() else {
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
        host.areas
            .workspace
            .auto_archive_tick(self, &db, now_ns, &mut report, cleanup_bound);
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
/// Dedicated blocking owner; retains the exact startup writer leases until joined.
enum OwnerCommand {
    Stop,
    #[cfg(any(test, feature = "test-support"))]
    Tick(i64, mpsc::Sender<RetentionReport>),
}
pub struct AutoArchiveOwner {
    producer: runtime::producer::ProducerGate,
    stop: mpsc::Sender<OwnerCommand>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl AutoArchiveOwner {
    pub fn start(
        host: Weak<Mutex<CommandHost>>,
        mut coordinator: AutoArchiveCoordinator,
        leases: Arc<storage::WriterLeases>,
        #[cfg(any(test, feature = "test-support"))] manual_fixture: bool,
    ) -> std::io::Result<Self> {
        let (stop, receive) = mpsc::channel();
        let producer = runtime::producer::ProducerGate::default();
        let admission = producer.clone();
        let worker = std::thread::Builder::new()
            .name("auto-archive".into())
            .spawn(move || {
                #[cfg(not(any(test, feature = "test-support")))]
                let manual_fixture = false;
                loop {
                    let message = if manual_fixture {
                        receive
                            .recv()
                            .map_err(|_| mpsc::RecvTimeoutError::Disconnected)
                    } else {
                        receive.recv_timeout(std::time::Duration::from_secs(5))
                    };
                    let now = match message {
                        Ok(OwnerCommand::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        #[cfg(any(test, feature = "test-support"))]
                        Ok(OwnerCommand::Tick(now, reply)) => {
                            let report = if let (Some(_permit), Some(host)) =
                                (admission.try_enter(), host.upgrade())
                            {
                                coordinator.tick(&host, leases.authority().as_ref(), now)
                            } else {
                                let mut report = RetentionReport::default();
                                report.hold("ProducerPaused");
                                report
                            };
                            let _ = reply.send(report);
                            continue;
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .ok()
                            .and_then(|d| i64::try_from(d.as_nanos()).ok()),
                    };
                    let Some(_permit) = admission.try_enter() else {
                        continue;
                    };
                    let Some(host) = host.upgrade() else {
                        break;
                    };
                    if let Some(now) = now {
                        coordinator.tick(&host, leases.authority().as_ref(), now);
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
    #[cfg(any(test, feature = "test-support"))]
    pub fn tick_for_test(&self, now_ns: i64) -> Result<RetentionReport, String> {
        let (reply, receive) = mpsc::channel();
        self.stop
            .send(OwnerCommand::Tick(now_ns, reply))
            .map_err(|_| "archive worker stopped")?;
        receive.recv().map_err(|_| "archive tick reply lost".into())
    }
    pub fn producer_gate(&self) -> runtime::producer::ProducerGate {
        self.producer.clone()
    }
    pub fn request_stop(&self) {
        self.producer.close();
        let _ = self.stop.send(OwnerCommand::Stop);
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
