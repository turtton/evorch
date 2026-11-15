use super::*;

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

    /// Only called after the enclosing dispatch scope and every internal tail returned.
    pub(super) fn reconcile(&self, queue: &LearningJobQueue) {
        let mut state = self.state.lock().unwrap();
        if state.unconfirmed_intake {
            let outcome = (|| {
                if let Some(job) = queue.job(&self.task_id)? {
                    if job.origin != *queue.origin() {
                        return Err(storage::StorageError::LearningWriteUnconfirmed);
                    }
                    if !job.state.is_terminal() {
                        state.key = queue.change(
                            &job.key(),
                            Change::Abandon(
                                storage::learning_jobs::LearningEndReason::UnconfirmedWrite,
                            ),
                        )?;
                        if state.key.is_none() {
                            return Err(storage::StorageError::LearningWriteUnconfirmed);
                        }
                    } else {
                        state.key = Some(job.key());
                    }
                }
                Ok::<(), storage::StorageError>(())
            })();
            if outcome.is_ok() {
                state.unconfirmed_intake = false;
                state.settle();
            }
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
                    // The held publish turned out to be impossible for its content: end the
                    // job through the failure terminal instead, still owned until it commits.
                    state.retry_terminal = Some((
                        key,
                        Change::Fail(storage::learning_jobs::LearningEndReason::PipelineFailed),
                    ));
                    continue;
                }
                _ => match queue.job(&key.task_id) {
                    // The job moved on (fenced or ended by someone with authority):
                    // adopt that state; never re-run a terminal on a stale key.
                    Ok(Some(job)) if job.origin == *queue.origin() && job.key() != key => {
                        state.key = Some(job.key());
                        state.retry_terminal = None;
                        state.settle();
                    }
                    _ => return,
                },
            }
            break;
        }
        let Some((key, change)) = state.uncertain.clone() else {
            return;
        };
        let outcome = (|| {
            let receipt = queue.receipt(&key, &change)?;
            let acknowledged = receipt.is_some();
            let next = if let Some(next) = receipt {
                next
            } else {
                queue
                    .job(&key.task_id)?
                    .ok_or(storage::StorageError::LearningWriteUnconfirmed)?
                    .key()
            };
            if next.owner != key.owner {
                return Err(storage::StorageError::LearningWriteUnconfirmed);
            }
            let exact_interrupt = acknowledged
                && matches!(change, Change::Interrupt { .. })
                && next.state == LearningJobState::Interrupted;
            let exact_requeue = acknowledged
                && matches!(change, Change::Requeue { .. })
                && next.state == LearningJobState::Pending;
            // A newer attempt owns the task now; this stale write must not end its job.
            let moved_on = self.superseded.load(Ordering::Acquire);
            if next.state.is_terminal() || exact_interrupt || exact_requeue || moved_on {
                return Ok(next);
            }
            queue
                .change(
                    &next,
                    Change::Abandon(storage::learning_jobs::LearningEndReason::UnconfirmedWrite),
                )?
                .ok_or(storage::StorageError::LearningWriteUnconfirmed)
        })();
        if let Ok(next) = outcome {
            state.key = Some(next);
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
