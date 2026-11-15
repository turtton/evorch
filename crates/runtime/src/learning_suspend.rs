//! Learning-only cancellation. Receipts describe normal returns AFTER dispatch scope,
//! not merely terminal run phases. Dropping a waiter never drops pipeline futures.
use std::collections::HashMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use storage::learning_jobs::{LearningAttemptChange as Change, LearningAttemptKey};
use tokio::sync::watch;

use crate::learning_jobs::{LearningJobQueue, LearningJobState};
use crate::producer::ProducerPause;
use crate::{AgentRuntime, RunId};

mod execution;

type Receipt = watch::Receiver<Option<Result<(), String>>>;

/// A returned entry whose terminal or readback write is unresolved is retried after
/// `BASE`, doubling up to `CAP`. At the cap it logs once and then keeps retrying every
/// `CAP`: ownership is never given up while the durable state is unknown.
const LEARNING_RETRY_BASE: std::time::Duration = std::time::Duration::from_secs(1);
const LEARNING_RETRY_CAP: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(Default)]
pub(crate) struct LearningExecutions {
    scheduler: AtomicBool,
    retrying: AtomicBool,
    /// Every entry that is not settled yet, by identity, in registration order. A task can
    /// have a running newer attempt and older entries whose cleanup is still unresolved.
    entries: Mutex<Vec<Arc<LearningExecution>>>,
    internal: Mutex<HashMap<RunId, watch::Sender<bool>>>,
    detached: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

#[derive(Default)]
struct ExecutionState {
    key: Option<LearningAttemptKey>,
    suspension: Option<String>,
    runs: Vec<RunId>,
    unsafe_result: Option<String>,
    uncertain: Option<(LearningAttemptKey, Change)>,
    /// A terminal change the writer rejected with a known rollback. The entry keeps
    /// owning the job until the same transaction commits (or the job is seen moved on).
    retry_terminal: Option<(LearningAttemptKey, Change)>,
    unconfirmed_intake: bool,
}

impl ExecutionState {
    /// The result is safe to report only when no unresolved write remains.
    fn settle(&mut self) {
        if !self.unconfirmed_intake && self.uncertain.is_none() && self.retry_terminal.is_none() {
            self.unsafe_result = None;
        }
    }
}

pub(crate) struct LearningExecution {
    pub(crate) task_id: String,
    state: Mutex<ExecutionState>,
    cancelled: AtomicBool,
    /// A newer attempt for the same task is registered: this entry only finishes its own
    /// cleanup and no longer represents the task.
    superseded: AtomicBool,
    returned: watch::Sender<Option<Result<(), String>>>,
    changed: watch::Sender<u64>,
}

impl LearningExecution {
    #[cfg(test)]
    pub(crate) fn returned_receipt(&self) -> Receipt {
        self.returned.subscribe()
    }
}

impl LearningExecutions {
    pub(crate) fn register(
        &self,
        task_id: &str,
        changed: watch::Sender<u64>,
    ) -> Option<Arc<LearningExecution>> {
        let mut entries = self.entries.lock().unwrap();
        // Only a running attempt blocks a new one; an earlier attempt that returned may still
        // be settling its own durable write.
        if entries
            .iter()
            .any(|e| e.task_id == task_id && e.returned.borrow().is_none())
        {
            return None;
        }
        for older in entries.iter().filter(|e| e.task_id == task_id) {
            older.superseded.store(true, Ordering::Release);
        }
        let entry = Arc::new(LearningExecution {
            task_id: task_id.into(),
            state: Mutex::default(),
            cancelled: AtomicBool::new(false),
            superseded: AtomicBool::new(false),
            returned: watch::channel(None).0,
            changed,
        });
        entries.push(entry.clone());
        Some(entry)
    }

    pub(crate) fn returned(&self, entry: &LearningExecution) {
        let result = entry
            .state
            .lock()
            .unwrap()
            .unsafe_result
            .clone()
            .map_or(Ok(()), Err);
        entry.returned.send_replace(Some(result.clone()));
        // Unknown durable outcomes remain visible; they must block the next seal.
        // Only this exact entry is released; a late callback never touches another attempt.
        if result.is_ok() {
            self.entries
                .lock()
                .unwrap()
                .retain(|e| !std::ptr::eq(Arc::as_ptr(e), entry));
        }
    }

    /// Every entry that returned but still holds an unresolved durable write, including
    /// those a newer attempt has superseded. This is the retry set.
    fn unresolved(&self) -> Vec<Arc<LearningExecution>> {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .filter(|entry| matches!(*entry.returned.borrow(), Some(Err(_))))
            .cloned()
            .collect()
    }

    /// The entries that stand for their task: what a suspend must wait for. A superseded
    /// entry is not part of it; its newer attempt is.
    fn cohort(&self) -> Vec<Arc<LearningExecution>> {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .filter(|entry| !entry.superseded.load(Ordering::Acquire))
            .cloned()
            .collect()
    }

    /// Puts a settled entry back because its cleanup is unresolved again. It never
    /// replaces another attempt: an older entry just joins the retry set as superseded.
    fn readmit(&self, entry: &Arc<LearningExecution>) {
        let mut entries = self.entries.lock().unwrap();
        if entries.iter().any(|e| Arc::ptr_eq(e, entry)) {
            return;
        }
        if entries.iter().any(|e| e.task_id == entry.task_id) {
            entry.superseded.store(true, Ordering::Release);
        }
        entries.push(entry.clone());
    }

    pub(crate) fn own(&self, task: tokio::task::JoinHandle<()>) {
        let mut tasks = self.detached.lock().unwrap();
        tasks.retain(|task| !task.is_finished());
        tasks.push(task);
    }

    pub(crate) fn internal_started(&self, run: RunId) {
        self.internal
            .lock()
            .unwrap()
            .insert(run, watch::channel(false).0);
    }

    pub(crate) fn internal_returned(&self, run: RunId) {
        if let Some(tail) = self.internal.lock().unwrap().get(&run) {
            tail.send_replace(true);
        }
    }
}

pub struct LearningPause {
    id: String,
    pause: ProducerPause,
}

pub struct LearningSuspension {
    id: String,
    entries: Vec<Arc<LearningExecution>>,
    recovery_taken: AtomicBool,
    fenced: Receipt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LearningSuspendWait {
    Complete,
    DeadlineExceeded,
}

impl LearningSuspension {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub async fn wait(&self) -> Result<(), String> {
        let mut fenced = self.fenced.clone();
        loop {
            if let Some(result) = fenced.borrow_and_update().clone() {
                result?;
                break;
            }
            fenced
                .changed()
                .await
                .map_err(|_| "learning fence owner lost")?;
        }
        for entry in &self.entries {
            let mut tail: Receipt = entry.returned.subscribe();
            loop {
                if let Some(result) = tail.borrow_and_update().clone() {
                    result?;
                    break;
                }
                tail.changed()
                    .await
                    .map_err(|_| "learning tail lost".to_string())?;
            }
        }
        Ok(())
    }

    async fn wait_cleanup(&self) {
        for entry in &self.entries {
            let mut tail = entry.returned.subscribe();
            loop {
                if matches!(*tail.borrow_and_update(), Some(Ok(()))) {
                    break;
                }
                // Errors remain owned until explicit durable readback reconciles them.
                if tail.changed().await.is_err() {
                    return;
                }
            }
        }
    }

    /// The one deadline covers the entire cohort. Only this notification future
    /// is timed out; runtime continues owning the tasks and their cleanup.
    pub async fn wait_until(
        &self,
        deadline: tokio::time::Instant,
    ) -> Result<LearningSuspendWait, String> {
        match tokio::time::timeout_at(deadline, self.wait()).await {
            Ok(result) => result.map(|()| LearningSuspendWait::Complete),
            Err(_) => Ok(LearningSuspendWait::DeadlineExceeded),
        }
    }
}

impl AgentRuntime {
    pub(crate) async fn learning_returned(&self, entry: &Arc<LearningExecution>) {
        self.reconcile_returned(entry).await;
        self.retry_unresolved_learning();
    }

    /// Keeps re-running the cleanup of returned entries whose terminal or readback write is
    /// still unresolved, so a database that recovers needs no operator or suspend request.
    /// Only the already-keyed change or receipt readback runs again: no provider call, and
    /// the entry stays owned until the durable state is settled.
    pub(crate) fn retry_unresolved_learning(&self) {
        let executions = &self.shared.learning_executions;
        if executions.unresolved().is_empty() || executions.retrying.swap(true, Ordering::AcqRel) {
            return;
        }
        let weak = Arc::downgrade(&self.shared);
        let task = tokio::spawn(async move {
            let mut delay = LEARNING_RETRY_BASE;
            let mut stalled = false;
            loop {
                tokio::time::sleep(delay).await;
                let Some(shared) = weak.upgrade() else { break };
                let runtime = AgentRuntime { shared };
                let executions = &runtime.shared.learning_executions;
                let pending = executions.unresolved();
                if pending.is_empty() {
                    executions.retrying.store(false, Ordering::Release);
                    // An entry can return between the scan and the release; if so, either
                    // that caller started a new task or this one carries on.
                    if executions.unresolved().is_empty()
                        || executions.retrying.swap(true, Ordering::AcqRel)
                    {
                        break;
                    }
                    delay = LEARNING_RETRY_BASE;
                    continue;
                }
                for entry in pending {
                    runtime.reconcile_returned(&entry).await;
                }
                if delay >= LEARNING_RETRY_CAP
                    && !stalled
                    && !runtime.shared.learning_executions.unresolved().is_empty()
                {
                    stalled = true;
                    tracing::error!(
                        interval_secs = LEARNING_RETRY_CAP.as_secs(),
                        "learning cleanup is still unresolved after repeated retries; it stays owned and keeps retrying"
                    );
                }
                delay = (delay * 2).min(LEARNING_RETRY_CAP);
            }
        });
        executions.own(task);
    }

    async fn reconcile_returned(&self, entry: &Arc<LearningExecution>) {
        if let Some(queue) = self.learning_job_queue() {
            let owned = entry.clone();
            if tokio::task::spawn_blocking(move || owned.reconcile(&queue))
                .await
                .is_err()
            {
                entry.state.lock().unwrap().unsafe_result =
                    Some("learning cleanup panicked".into());
            }
        }
        self.shared.learning_executions.returned(entry);
        self.shared
            .learning_jobs_changed
            .send_modify(|n| *n = n.wrapping_add(1));
    }

    /// Retry readback/terminal cleanup only; never starts a provider or grants retry authority.
    pub async fn reconcile_learning_suspension(&self, suspension: &LearningSuspension) {
        for entry in &suspension.entries {
            if entry.returned.borrow().is_some() {
                self.learning_returned(entry).await;
            }
        }
    }

    pub fn pause_learning(&self) -> LearningPause {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        LearningPause {
            id: format!(
                "{}-{}",
                self.learning_origin().instance,
                NEXT.fetch_add(1, Ordering::Relaxed)
            ),
            pause: self.learning_gate().pause(),
        }
    }

    /// Call after evaluation drain, before runtime quiesce. No provider response is awaited.
    pub fn suspend_learning(&self, pause: &LearningPause) -> Result<LearningSuspension, String> {
        if !self.learning_gate().is_paused_by(pause.pause) {
            return Err("learning pause is stale or foreign".into());
        }
        let entries = self.shared.learning_executions.cohort();
        let (fenced, fence_receipt) = watch::channel(None);
        let runtime = self.clone();
        let cohort = entries.clone();
        let id = pause.id.clone();
        let task = tokio::spawn(async move {
            // The single cohort deadline includes durable I/O. A blocked SQLite
            // writer must not block the runtime timer or drop any owned cleanup.
            let result = tokio::task::spawn_blocking(move || {
                if let Some(queue) = runtime.learning_job_queue() {
                    for entry in &cohort {
                        entry.interrupt(&queue, &runtime, &id);
                    }
                }
            })
            .await
            .map_err(|_| "learning fence task panicked".to_string());
            fenced.send_replace(Some(result));
        });
        self.shared.learning_executions.own(task);
        Ok(LearningSuspension {
            id: pause.id.clone(),
            entries,
            recovery_taken: AtomicBool::new(false),
            fenced: fence_receipt,
        })
    }

    /// Before suspend, cancellation simply reopens admission and dispatches durable Pending.
    pub fn resume_learning(&self, pause: LearningPause) -> bool {
        if !self.learning_gate().resume(pause.pause) {
            return false;
        }
        self.dispatch_pending_learning();
        true
    }

    /// 4b calls ONLY after cancellation is confirmed before capture. It must not
    /// call this on exec failure. Cleanup cannot reopen another request's gate.
    pub fn cancel_learning_suspension(
        &self,
        pause: LearningPause,
        suspension: Arc<LearningSuspension>,
    ) -> Result<(), String> {
        if self.quiesce_state() != crate::quiesce::QuiesceState::Running {
            return Err(
                "learning retry requires a confirmed pre-capture restart cancellation".into(),
            );
        }
        if pause.id != suspension.id || suspension.recovery_taken.swap(true, Ordering::AcqRel) {
            return Err("learning suspension cancellation mismatch".into());
        }
        if !self.resume_learning(pause) {
            return Err("learning pause is stale".into());
        }
        let runtime = self.clone();
        let task = tokio::spawn(async move {
            suspension.wait_cleanup().await;
            let Some(queue) = runtime.learning_job_queue() else {
                return;
            };
            for entry in &suspension.entries {
                let key = entry.state.lock().unwrap().key.clone();
                if let Some(key) = key
                    && key.state == LearningJobState::Interrupted
                {
                    let change = if queue.origin().kind
                        == storage::learning_jobs::LearningOriginKind::Core
                    {
                        Change::Requeue {
                            suspension_id: suspension.id.clone(),
                            to: queue.origin().clone(),
                        }
                    } else {
                        Change::Abandon(
                            storage::learning_jobs::LearningEndReason::NoResumeAuthority,
                        )
                    };
                    let writer = queue.clone();
                    let expected = key.clone();
                    let command = change.clone();
                    let result =
                        tokio::task::spawn_blocking(move || writer.change(&expected, command))
                            .await;
                    match result {
                        Ok(Ok(Some(next))) => entry.state.lock().unwrap().key = Some(next),
                        _ => {
                            {
                                let mut state = entry.state.lock().unwrap();
                                state.unsafe_result = Some("learning requeue unconfirmed".into());
                                state.uncertain = Some((key, change));
                            }
                            runtime.shared.learning_executions.readmit(entry);
                            runtime.learning_returned(entry).await;
                            let mut tail = entry.returned.subscribe();
                            loop {
                                if matches!(*tail.borrow_and_update(), Some(Ok(()))) {
                                    break;
                                }
                                if tail.changed().await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                }
            }
            runtime
                .shared
                .learning_jobs_changed
                .send_modify(|n| *n = n.wrapping_add(1));
            runtime.dispatch_pending_learning();
        });
        self.shared.learning_executions.own(task);
        Ok(())
    }

    pub(crate) async fn wait_learning_internal_tail(&self, run: RunId) -> Result<(), String> {
        let mut tail = self
            .shared
            .learning_executions
            .internal
            .lock()
            .unwrap()
            .get(&run)
            .ok_or("learning internal tail missing")?
            .subscribe();
        while !*tail.borrow_and_update() {
            tail.changed()
                .await
                .map_err(|_| "learning internal tail lost")?;
        }
        self.shared
            .learning_executions
            .internal
            .lock()
            .unwrap()
            .remove(&run);
        Ok(())
    }

    pub(crate) fn ensure_learning_scheduler(&self) {
        if self
            .shared
            .learning_executions
            .scheduler
            .swap(true, Ordering::AcqRel)
        {
            return;
        }
        let weak = Arc::downgrade(&self.shared);
        let mut learning = self.learning_gate().subscribe_admission();
        let mut producers = self.producer_gate().subscribe_admission();
        let mut jobs = self.subscribe_learning_changes();
        let task = tokio::spawn(async move {
            loop {
                let gate_changed = tokio::select! {
                    result = learning.changed() => { if result.is_err() { break } true },
                    result = producers.changed() => { if result.is_err() { break } true },
                    result = jobs.changed() => { if result.is_err() { break } false },
                };
                let Some(shared) = weak.upgrade() else { break };
                let runtime = AgentRuntime { shared };
                if gate_changed {
                    runtime
                        .shared
                        .learning_jobs_changed
                        .send_modify(|n| *n = n.wrapping_add(1));
                }
                runtime.dispatch_pending_learning();
                runtime.retry_unresolved_learning();
                if runtime.learning_gate().is_closed() || runtime.producer_gate().is_closed() {
                    break;
                }
            }
        });
        self.shared.learning_executions.own(task);
    }

    fn dispatch_pending_learning(&self) {
        if !self.learning_gate().is_open() || !self.producer_gate().is_open() {
            return;
        }
        let Some(queue) = self.learning_job_queue() else {
            return;
        };
        let jobs = storage::Database::open(&self.shared.learning.get().unwrap().storage)
            .and_then(|db| db.learning_jobs(&[LearningJobState::Pending]));
        let Ok(jobs) = jobs else { return };
        for job in jobs {
            if job.origin != *queue.origin() {
                continue;
            }
            match queue.evidence(&job.task_id) {
                Ok(Some(evidence)) => {
                    let _ = self.resume_adopted_learning(crate::learning_jobs::AdoptedJob {
                        job,
                        evidence,
                    });
                }
                _ => {
                    let _ = queue.abandon_observed(&job.key());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
