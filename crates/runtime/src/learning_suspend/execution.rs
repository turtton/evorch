use super::*;

/// The outcome of re-reading a job to settle a write whose result is unknown.
enum Reread {
    /// The job moved on to a state this entry must not write over (ended, another owner,
    /// or reserved for requeue). Carries the job's key, if the job still exists.
    Settled(Option<LearningAttemptKey>),
    /// The job still has exactly this entry's key: the write never applied.
    Unapplied,
    /// An own-origin Started job that is not this entry's attempt. Never written over.
    Foreign,
}

impl LearningExecution {
    // SQLite/writer ACKs and bind+spawn can block: do not hold a Tokio worker,
    // so the parent deadline and cooperative provider cancellations stay runnable.
    pub(crate) async fn start_async(
        self: &Arc<Self>,
        queue: &LearningJobQueue,
        permit: &Arc<crate::producer::ProducerPermit>,
    ) -> Result<bool, String> {
        let (entry, queue, permit) = (self.clone(), queue.clone(), permit.clone());
        tokio::task::spawn_blocking(move || entry.start(&queue, &permit))
            .await
            .map_err(|_| "learning start task failed")?
    }
    pub(crate) async fn change_async(
        self: &Arc<Self>,
        queue: &LearningJobQueue,
        change: Change,
    ) -> Result<bool, String> {
        let (entry, queue) = (self.clone(), queue.clone());
        tokio::task::spawn_blocking(move || entry.change(&queue, change))
            .await
            .map_err(|_| "learning write task failed")?
    }
    pub(crate) async fn spawn_async(
        self: &Arc<Self>,
        queue: &LearningJobQueue,
        run: RunId,
        review: bool,
        spawn: impl FnOnce() + Send + 'static,
    ) -> Result<bool, String> {
        let (entry, queue) = (self.clone(), queue.clone());
        tokio::task::spawn_blocking(move || entry.spawn(&queue, run, review, spawn))
            .await
            .map_err(|_| "learning spawn task failed")?
    }

    pub(crate) fn start(
        &self,
        queue: &LearningJobQueue,
        _permit: &crate::producer::ProducerPermit,
    ) -> Result<bool, String> {
        let mut state = self.state.lock().unwrap();
        if state.suspension.is_some() {
            return Ok(false);
        }
        let Some(job) = queue.job(&self.task_id).map_err(|_| {
            state.unsafe_result = Some("learning intake unconfirmed".into());
            state.unconfirmed_intake = true;
            "learning start read failed"
        })?
        else {
            return Ok(false);
        };
        if job.origin != *queue.origin() || job.state != LearningJobState::Pending {
            return Ok(false);
        }
        let key = job.key();
        match Self::apply(&mut state, queue, &key, Change::Start)? {
            Some(next) => {
                let started = next.state == LearningJobState::Started;
                state.key = Some(next);
                self.changed.send_modify(|n| *n = n.wrapping_add(1));
                Ok(started)
            }
            None => Ok(false),
        }
    }

    fn apply(
        state: &mut ExecutionState,
        queue: &LearningJobQueue,
        key: &LearningAttemptKey,
        change: Change,
    ) -> Result<Option<LearningAttemptKey>, String> {
        if state.unsafe_result.is_some() {
            return Err("learning cleanup is unresolved".into());
        }
        match queue.change(key, change.clone()) {
            Ok(result) => Ok(result),
            Err(storage::StorageError::LearningWriteUnconfirmed) => {
                state.unsafe_result = Some("learning write outcome unconfirmed".into());
                state.uncertain = Some((key.clone(), change));
                Err("learning write outcome unconfirmed".into())
            }
            Err(error) => {
                // A publish the content itself makes impossible (SecretGuard, an identity
                // clash) is final: the caller ends the job through the failure terminal.
                let content_rejected =
                    matches!(change, Change::Publish) && error.is_content_rejection();
                if !content_rejected
                    && matches!(
                        change,
                        Change::Publish | Change::Fail(_) | Change::Abandon(_)
                    )
                {
                    // A rejected terminal write leaves Started + evidence behind. Keep
                    // owning it so a later reconcile re-runs the same transaction.
                    state.unsafe_result = Some("learning terminal write rejected".into());
                    state.retry_terminal = Some((key.clone(), change));
                }
                Err("learning write rejected".into())
            }
        }
    }

    pub(crate) fn change(&self, queue: &LearningJobQueue, change: Change) -> Result<bool, String> {
        let mut state = self.state.lock().unwrap();
        if state.suspension.is_some() {
            return Ok(false);
        }
        let Some(key) = state.key.clone() else {
            return Ok(false);
        };
        match Self::apply(&mut state, queue, &key, change)? {
            Some(next) => {
                state.key = Some(next);
                self.changed.send_modify(|n| *n = n.wrapping_add(1));
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Bind and spawn while holding the same lock suspend uses to fence/cancel.
    pub(crate) fn spawn(
        &self,
        queue: &LearningJobQueue,
        run: RunId,
        review: bool,
        spawn: impl FnOnce(),
    ) -> Result<bool, String> {
        let mut state = self.state.lock().unwrap();
        if state.suspension.is_some() {
            return Ok(false);
        }
        let Some(key) = state.key.clone() else {
            return Ok(false);
        };
        match Self::apply(
            &mut state,
            queue,
            &key,
            Change::BindRun {
                run: run.to_string(),
                review,
            },
        )? {
            Some(next) => {
                state.key = Some(next);
                spawn();
                state.runs.push(run);
                self.changed.send_modify(|n| *n = n.wrapping_add(1));
                Ok(true)
            }
            None => Ok(false),
        }
    }

    pub(crate) fn interrupted(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub(crate) fn intake_unconfirmed(&self) {
        let mut state = self.state.lock().unwrap();
        state.unconfirmed_intake = true;
        state.unsafe_result = Some("learning intake unconfirmed".into());
    }

    /// What the job says about a write this entry cannot confirm. Reading never writes, and
    /// a key read back from the database is never reused to write on this entry's behalf.
    fn reread(
        queue: &LearningJobQueue,
        task_id: &str,
        own: &LearningAttemptKey,
    ) -> Result<Reread, storage::StorageError> {
        let Some(job) = queue.job(task_id)? else {
            return Ok(Reread::Settled(None));
        };
        if job.key() == *own {
            return Ok(Reread::Unapplied);
        }
        if job.state.is_terminal()
            || job.origin != *queue.origin()
            || matches!(
                job.state,
                LearningJobState::Pending | LearningJobState::Interrupted
            )
        {
            return Ok(Reread::Settled(Some(job.key())));
        }
        tracing::error!(task = %task_id, "own-origin started job is not this entry's attempt; keeping it unresolved");
        Ok(Reread::Foreign)
    }

    /// An entry without an attempt may end only the Pending job it observed, by CAS. Any
    /// other state belongs to someone else and is settled without a write.
    fn settle_intake(
        &self,
        state: &mut ExecutionState,
        queue: &LearningJobQueue,
    ) -> Result<(), storage::StorageError> {
        let Some(job) = queue.job(&self.task_id)? else {
            return Ok(());
        };
        if job.origin == *queue.origin() && job.state == LearningJobState::Pending {
            if let Some(next) = queue.change(
                &job.key(),
                Change::Abandon(storage::learning_jobs::LearningEndReason::UnconfirmedWrite),
            )? {
                state.key = Some(next);
                return Ok(());
            }
            // The Pending key moved while we looked: it is not ours to end.
            return match queue.job(&self.task_id)? {
                Some(job) => {
                    state.key = Some(job.key());
                    Ok(())
                }
                None => Ok(()),
            };
        }
        if job.state.is_terminal() {
            state.key = Some(job.key());
        }
        Ok(())
    }

    /// Only called after the enclosing dispatch scope and every internal tail returned.
    pub(super) fn reconcile(&self, queue: &LearningJobQueue) {
        let mut state = self.state.lock().unwrap();
        if state.unconfirmed_intake && self.settle_intake(&mut state, queue).is_ok() {
            state.unconfirmed_intake = false;
            state.settle();
        }
        while let Some((key, change)) = state.retry_terminal.clone() {
            match queue.change(&key, change.clone()) {
                Ok(Some(next)) => {
                    // Any fence/readback that was pending is superseded by the commit.
                    state.key = Some(next);
                    state.retry_terminal = None;
                    state.uncertain = None;
                    state.settle();
                    return;
                }
                Err(storage::StorageError::LearningWriteUnconfirmed) => {
                    state.retry_terminal = None;
                    state.uncertain = Some((key, change));
                }
                Err(error) if matches!(change, Change::Publish) && error.is_content_rejection() => {
                    state.retry_terminal = Some((
                        key,
                        Change::Fail(storage::learning_jobs::LearningEndReason::PipelineFailed),
                    ));
                    continue;
                }
                _ => {
                    if let Ok(Reread::Settled(next)) = Self::reread(queue, &self.task_id, &key) {
                        if next.is_some() {
                            state.key = next;
                        }
                        state.retry_terminal = None;
                        state.settle();
                    }
                }
            }
            break;
        }
        let Some((key, change)) = state.uncertain.clone() else {
            return;
        };
        let outcome = (|| {
            let receipt = queue.receipt(&key, &change)?;
            let acknowledged = receipt.is_some();
            let own = match receipt {
                Some(own) => own,
                None => match Self::reread(queue, &self.task_id, &key)? {
                    Reread::Unapplied => key.clone(),
                    Reread::Settled(next) => return Ok(next),
                    Reread::Foreign => return Err(storage::StorageError::LearningWriteUnconfirmed),
                },
            };
            let exact_interrupt = acknowledged
                && matches!(change, Change::Interrupt { .. })
                && own.state == LearningJobState::Interrupted;
            let exact_requeue = acknowledged
                && matches!(change, Change::Requeue { .. })
                && own.state == LearningJobState::Pending;
            if own.state.is_terminal() || exact_interrupt || exact_requeue {
                return Ok(Some(own));
            }
            match queue.change(
                &own,
                Change::Abandon(storage::learning_jobs::LearningEndReason::UnconfirmedWrite),
            )? {
                Some(next) => Ok(Some(next)),
                None => match Self::reread(queue, &self.task_id, &own)? {
                    Reread::Settled(next) => Ok(next),
                    _ => Err(storage::StorageError::LearningWriteUnconfirmed),
                },
            }
        })();
        if let Ok(next) = outcome {
            if next.is_some() {
                state.key = next;
            }
            state.uncertain = None;
            state.settle();
        }
    }

    pub(super) fn interrupt(&self, queue: &LearningJobQueue, runtime: &AgentRuntime, id: &str) {
        let mut state = self.state.lock().unwrap();
        if state.suspension.is_some() {
            // A later request joins the still-owned cleanup; it never changes
            // that cleanup's suspension identity or reopens its admission gate.
            return;
        }
        if state
            .key
            .as_ref()
            .is_some_and(|key| key.state.is_terminal())
        {
            return;
        }
        // Local fence first: even a failed durable fence must stop this task's
        // provider work/publication. It remains owned and cannot authorize a seal.
        self.cancelled.store(true, Ordering::Release);
        state.suspension = Some(id.into());
        if let Some(key) = state.key.clone()
            && key.state == LearningJobState::Started
        {
            let change = Change::Interrupt {
                suspension_id: id.into(),
            };
            match Self::apply(&mut state, queue, &key, change.clone()) {
                Ok(Some(next)) => state.key = Some(next),
                Ok(None) => {
                    state.unsafe_result = Some("learning fence rejected".into());
                    state.uncertain = Some((key, change));
                }
                Err(_) => {
                    state.unsafe_result = Some("learning fence unconfirmed".into());
                    state.uncertain.get_or_insert((key, change));
                }
            }
        }
        for run in &state.runs {
            if runtime.inspect_agent(*run).map_or(true, |a| {
                !matches!(
                    a.phase,
                    crate::AgentRunPhase::Done
                        | crate::AgentRunPhase::Error
                        | crate::AgentRunPhase::Stopped
                )
            }) && runtime.cancel(*run).is_err()
            {
                state.unsafe_result = Some("learning cancellation rejected".into());
                break;
            }
        }
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
    }
}
