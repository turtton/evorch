//! Durable learning queue for the runtime (07b-2): job admission, the `Pending ->
//! Started` compare-and-set inside a producer permit, restart-time rescans, the shared
//! janitor for dead origins, and the bounded learning drain.
//!
//! Nothing here kills a learning pipeline. A drain that hits its limit reports
//! `TimedOut` and leaves every pipeline running; the caller decides what to cancel.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use storage::learning_jobs::NewLearningJob;
pub use storage::learning_jobs::{
    LearningAdmission, LearningJob, LearningJobEvidence, LearningJobState, LearningOrigin,
    LearningOriginKind,
};
use storage::{Database, StorageConfig, StorageError, StorageHandle};

use crate::AgentRuntime;
use crate::producer::{ProducerGate, ProducerPause, ProducerPermit};

/// Whether the process that owns a job is still running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OriginLiveness {
    Alive,
    Dead,
    /// Nothing proves either; a job of such an origin is neither run nor cleaned up.
    Unknown,
}

pub trait OriginProbe: Send + Sync {
    fn liveness(&self, origin: &LearningOrigin) -> OriginLiveness;
}

/// Random per process image. `exec` keeps the pid and the start time but replaces the
/// image, so a static is born again in the successor and its predecessor's instance
/// differs; every origin made inside one image shares it.
fn process_instance() -> &'static str {
    static INSTANCE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    INSTANCE.get_or_init(|| {
        let mut nonce = [0_u8; 16];
        if getrandom::fill(&mut nonce).is_err() {
            // Without entropy, still tell images apart by their start time and address.
            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos());
            nonce = (now ^ (u128::from(std::process::id()) << 96)).to_le_bytes();
        }
        nonce.iter().map(|byte| format!("{byte:02x}")).collect()
    })
}

/// Identity of the current process image. Every call in one image names the same
/// instance, so a live process never judges its own origins dead; an `exec`
/// successor gets a new instance and sees its predecessor's as dead.
pub fn current_origin(kind: LearningOriginKind, config_identity: Option<String>) -> LearningOrigin {
    let pid = std::process::id();
    LearningOrigin {
        kind,
        instance: process_instance().to_owned(),
        pid,
        start_ticks: read_start_ticks(&PathBuf::from("/proc"), &pid.to_string()),
        boot_id: read_trimmed(&PathBuf::from("/proc/sys/kernel/random/boot_id")),
        pid_ns: std::fs::read_link("/proc/self/ns/pid")
            .ok()
            .map(|path| path.display().to_string()),
        config_identity,
    }
}

fn read_trimmed(path: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// Field 22 of `/proc/<pid>/stat`. The command name may contain spaces and
/// parentheses, so fields are counted after its last closing parenthesis.
fn start_ticks_of(stat: &str) -> Option<u64> {
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

fn read_start_ticks(proc_root: &std::path::Path, pid: &str) -> Option<u64> {
    start_ticks_of(&std::fs::read_to_string(proc_root.join(pid).join("stat")).ok()?)
}

/// Judges liveness from `/proc` against the identity of the current incarnation.
pub struct ProcOriginProbe {
    me: LearningOrigin,
    proc_root: PathBuf,
}

impl ProcOriginProbe {
    pub fn new(me: &LearningOrigin) -> Self {
        Self::with_proc_root(me, PathBuf::from("/proc"))
    }

    /// A `/proc` lookalike, for tests that cannot spawn the processes they describe.
    pub fn with_proc_root(me: &LearningOrigin, proc_root: PathBuf) -> Self {
        Self {
            me: me.clone(),
            proc_root,
        }
    }
}

impl OriginProbe for ProcOriginProbe {
    fn liveness(&self, origin: &LearningOrigin) -> OriginLiveness {
        use OriginLiveness::{Alive, Dead, Unknown};
        if origin.instance == self.me.instance {
            return Alive;
        }
        let (Some(theirs), Some(ours)) = (&origin.boot_id, &self.me.boot_id) else {
            return Unknown;
        };
        if theirs != ours {
            return Dead;
        }
        // A pid this namespace cannot see says nothing about the process.
        if origin.pid_ns.is_none() || origin.pid_ns != self.me.pid_ns {
            return Unknown;
        }
        let Some(started) = origin.start_ticks else {
            return Unknown;
        };
        if origin.pid == self.me.pid && Some(started) == self.me.start_ticks {
            // The same process under an earlier instance: it `exec`ed into this one.
            return Dead;
        }
        match std::fs::read_to_string(self.proc_root.join(origin.pid.to_string()).join("stat")) {
            Ok(stat) => match start_ticks_of(&stat) {
                Some(ticks) if ticks == started => Alive,
                Some(_) => Dead,
                None => Unknown,
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Dead,
            Err(_) => Unknown,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LearningQueueError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("only a core may take over learning jobs")]
    NotCore,
}

/// What admission needs; the origin is the queue's own.
#[derive(Debug, Clone)]
pub struct AdmitRequest {
    pub task_id: String,
    pub roots: Vec<String>,
    pub goal: bool,
    pub daily_limit: u32,
    pub project: String,
    pub source_run_id: String,
    pub prompt: String,
    pub evidence_json: String,
}

/// A job a core took over, with its verified evidence.
#[derive(Debug)]
pub struct AdoptedJob {
    pub job: LearningJob,
    pub evidence: LearningJobEvidence,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub abandoned_started: usize,
    pub abandoned_gui_pending: usize,
    /// Pending jobs of a dead core, left for a core of the same config to take over.
    pub left_for_core: usize,
    pub origin_alive: usize,
    pub origin_unknown: usize,
}

type Clock = Arc<dyn Fn() -> SystemTime + Send + Sync>;

#[derive(Clone)]
pub struct LearningJobQueue {
    writer: StorageHandle,
    storage: StorageConfig,
    origin: LearningOrigin,
    clock: Clock,
}

impl LearningJobQueue {
    pub fn new(writer: StorageHandle, storage: StorageConfig, origin: LearningOrigin) -> Self {
        Self {
            writer,
            storage,
            origin,
            clock: Arc::new(SystemTime::now),
        }
    }

    /// Replaces the wall clock that stamps admission days and transitions.
    #[must_use]
    pub fn with_clock(mut self, clock: impl Fn() -> SystemTime + Send + Sync + 'static) -> Self {
        self.clock = Arc::new(clock);
        self
    }

    pub fn origin(&self) -> &LearningOrigin {
        &self.origin
    }

    fn now(&self) -> SystemTime {
        (self.clock)()
    }

    /// Dedup claim, `Pending` job and immutable evidence in one transaction.
    pub fn admit(&self, request: AdmitRequest) -> Result<LearningAdmission, StorageError> {
        self.writer.admit_learning_job(
            NewLearningJob {
                task_id: request.task_id,
                roots: request.roots,
                goal: request.goal,
                daily_limit: request.daily_limit,
                origin: self.origin.clone(),
                project: request.project,
                source_run_id: request.source_run_id,
                prompt: request.prompt,
                evidence_json: request.evidence_json,
            },
            self.now(),
        )
    }

    /// `Pending -> Started`. Taking the producer permit as an argument makes a start
    /// outside the learning gate unrepresentable. If this fails with an error the
    /// start may or may not be recorded: end the job with [`Self::abandon`], never
    /// run it.
    pub fn start(&self, task_id: &str, _permit: &ProducerPermit) -> Result<bool, StorageError> {
        self.writer
            .start_learning_job(task_id, &self.origin.instance, self.now())
    }

    pub fn finish(&self, task_id: &str, ok: bool) -> Result<bool, StorageError> {
        self.writer
            .finish_learning_job(task_id, &self.origin.instance, ok, self.now())
    }

    pub fn abandon(&self, task_id: &str, reason: &str) -> Result<bool, StorageError> {
        self.writer
            .abandon_learning_job(task_id, &self.origin.instance, reason, self.now())
    }

    pub fn job(&self, task_id: &str) -> Result<Option<LearningJob>, StorageError> {
        Database::open(&self.storage)?.learning_job(task_id)
    }

    /// Jobs of this incarnation that are started and not yet ended.
    pub fn started_by_me(&self) -> Result<Vec<LearningJob>, StorageError> {
        Ok(Database::open(&self.storage)?
            .learning_jobs(&[LearningJobState::Started])?
            .into_iter()
            .filter(|job| job.origin.instance == self.origin.instance)
            .collect())
    }

    /// Takes over the `Pending` jobs of dead cores of the same config. Only a core
    /// does this, and it never takes a GUI job, a live origin's job, a job whose
    /// origin cannot be judged, or a job that has started.
    pub fn rescan_core(
        &self,
        probe: &dyn OriginProbe,
    ) -> Result<Vec<AdoptedJob>, LearningQueueError> {
        if self.origin.kind != LearningOriginKind::Core || self.origin.config_identity.is_none() {
            return Err(LearningQueueError::NotCore);
        }
        let database = Database::open(&self.storage)?;
        let mut adopted = Vec::new();
        for job in database.learning_jobs(&[LearningJobState::Pending])? {
            if job.origin.kind != LearningOriginKind::Core
                || job.origin.config_identity != self.origin.config_identity
                || job.origin.instance == self.origin.instance
                || probe.liveness(&job.origin) != OriginLiveness::Dead
            {
                continue;
            }
            if !self.writer.adopt_learning_job(
                &job.task_id,
                &job.origin.instance,
                job.revision,
                self.origin.clone(),
            )? {
                continue;
            }
            match database.learning_job_evidence(&job.task_id) {
                Ok(Some(evidence)) => {
                    let job = database.learning_job(&job.task_id)?.unwrap_or(job);
                    adopted.push(AdoptedJob { job, evidence });
                }
                Ok(None) | Err(_) => {
                    // Without verified evidence the job cannot run; ending it keeps
                    // the claim and the quota spent, like any other ended job.
                    self.abandon(&job.task_id, "evidence unavailable")?;
                }
            }
        }
        Ok(adopted)
    }

    /// The shared janitor: ends what a dead origin left behind. A `Started` job is
    /// ended, never run again; a dead GUI's `Pending` job is ended because no core
    /// may run it. A dead core's `Pending` job stays for [`Self::rescan_core`].
    /// An origin that is alive or cannot be judged is left alone.
    pub fn sweep_dead_origins(&self, probe: &dyn OriginProbe) -> Result<SweepReport, StorageError> {
        let database = Database::open(&self.storage)?;
        let mut report = SweepReport::default();
        for job in
            database.learning_jobs(&[LearningJobState::Pending, LearningJobState::Started])?
        {
            if job.origin.instance == self.origin.instance {
                continue;
            }
            match probe.liveness(&job.origin) {
                OriginLiveness::Alive => report.origin_alive += 1,
                OriginLiveness::Unknown => report.origin_unknown += 1,
                OriginLiveness::Dead => {
                    let (reason, counter) = match (job.state, job.origin.kind) {
                        (LearningJobState::Started, _) => (
                            "origin died after the job started",
                            &mut report.abandoned_started,
                        ),
                        (_, LearningOriginKind::Gui) => (
                            "gui origin died before the job started",
                            &mut report.abandoned_gui_pending,
                        ),
                        _ => {
                            report.left_for_core += 1;
                            continue;
                        }
                    };
                    // Guarded by the dead instance: a job someone took over meanwhile
                    // no longer matches and is left alone.
                    if self.writer.abandon_learning_job(
                        &job.task_id,
                        &job.origin.instance,
                        reason,
                        self.now(),
                    )? {
                        *counter += 1;
                    }
                }
            }
        }
        Ok(report)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LearningDrainOutcome {
    /// No pipeline of this incarnation is running or recorded as started.
    Drained,
    /// The limit came first. Nothing was stopped; the pipelines run on.
    TimedOut,
    Failed(String),
}

/// Pauses new learning pipelines (their jobs stay `Pending`, durably) and waits for
/// the ones already started. Dropping it without [`Self::resume`] leaves the pause in
/// place, as a seal that goes on to exec must.
pub struct LearningDrain {
    gate: ProducerGate,
    pause: ProducerPause,
    queue: Option<LearningJobQueue>,
}

impl LearningDrain {
    /// Waits until the started pipelines end or `deadline` completes, whichever is
    /// first; an already finished drain wins a tie.
    pub async fn wait_until(&self, deadline: impl Future<Output = ()>) -> LearningDrainOutcome {
        if self.gate.is_closed() {
            return LearningDrainOutcome::Failed("learning gate is closed".into());
        }
        tokio::select! {
            biased;
            () = self.gate.drained() => self.verify_drained(),
            () = deadline => LearningDrainOutcome::TimedOut,
        }
    }

    pub async fn wait_for(&self, limit: Duration) -> LearningDrainOutcome {
        self.wait_until(tokio::time::sleep(limit)).await
    }

    fn verify_drained(&self) -> LearningDrainOutcome {
        let Some(queue) = &self.queue else {
            return LearningDrainOutcome::Drained;
        };
        match queue.started_by_me() {
            Ok(started) if started.is_empty() => LearningDrainOutcome::Drained,
            Ok(started) => LearningDrainOutcome::Failed(format!(
                "{} learning job(s) are recorded as started without a running pipeline",
                started.len()
            )),
            Err(error) => LearningDrainOutcome::Failed(error.to_string()),
        }
    }

    /// Lets new pipelines start again. `false` if a later pause or a shutdown owns
    /// the gate now.
    pub fn resume(self) -> bool {
        self.gate.resume(self.pause)
    }
}

impl AgentRuntime {
    /// The gate learning pipelines start behind. It is not the shared producer gate:
    /// pausing it stops learning only, never lanes or chats.
    pub fn learning_gate(&self) -> ProducerGate {
        self.shared.learning_gate.clone()
    }

    /// Declares the origin of this process's learning jobs. Without it they are
    /// attributed to an unnamed GUI, which no core ever runs again.
    #[must_use]
    pub fn with_learning_origin(self, origin: LearningOrigin) -> Self {
        let _ = self.shared.learning_origin.set(origin);
        self
    }

    pub(crate) fn learning_origin(&self) -> LearningOrigin {
        self.shared
            .learning_origin
            .get_or_init(|| current_origin(LearningOriginKind::Gui, None))
            .clone()
    }

    /// `None` until [`Self::with_learning`] configured learning.
    pub fn learning_job_queue(&self) -> Option<LearningJobQueue> {
        let settings = self.shared.learning.get()?;
        Some(LearningJobQueue::new(
            settings.writer.clone(),
            settings.storage.clone(),
            self.learning_origin(),
        ))
    }

    /// Stops new learning pipelines from starting; the caller then waits on the
    /// returned drain.
    pub fn begin_learning_drain(&self) -> LearningDrain {
        let gate = self.learning_gate();
        let pause = gate.pause();
        LearningDrain {
            gate,
            pause,
            queue: self.learning_job_queue(),
        }
    }

    /// The shared janitor against the live `/proc`: ends the jobs dead origins left
    /// behind, never runs one, and leaves an origin it cannot judge alone. Both the
    /// core and the GUI bootstraps call it. `None` when learning is not configured.
    pub fn sweep_dead_learning_origins_from_proc(
        &self,
    ) -> Option<Result<SweepReport, StorageError>> {
        let queue = self.learning_job_queue()?;
        let probe = ProcOriginProbe::new(queue.origin());
        Some(queue.sweep_dead_origins(&probe))
    }

    /// Runs the shared janitor once. `None` when learning is not configured.
    pub fn sweep_dead_learning_origins(
        &self,
        probe: &dyn OriginProbe,
    ) -> Option<Result<SweepReport, StorageError>> {
        Some(self.learning_job_queue()?.sweep_dead_origins(probe))
    }
}

#[cfg(test)]
mod tests;
