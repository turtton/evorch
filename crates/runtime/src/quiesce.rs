//! Safe-point capture, not operator stop and not a restore authority (ADR 0045 §8).
//! Restarting/restoring are subsequent transactions owned by the core host.
use crate::{RunId, RunStore};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, MutexGuard};
use tokio::sync::watch;

#[cfg(test)]
mod fixture;
#[cfg(test)]
mod recovery_06a_tests;
#[cfg(test)]
mod recovery_06b_tests;
#[cfg(test)]
mod recovery_cache_tests;
mod restart;
pub(crate) mod runtime;
mod safe_point;
pub use restart::{RestartAttempt, ResumeReceipt};
#[cfg(test)]
mod restart_tests;
#[cfg(test)]
mod tests;

mod schema;
pub use schema::*;
pub(crate) mod dispatch;
pub use dispatch::DispatchDrainFence;
mod recovery;
pub use recovery::*;
mod continuity;
pub use continuity::*;

pub(crate) struct Coordinator {
    inner: Mutex<Inner>,
    version: watch::Sender<u64>,
    pub(crate) checkpoint_gate: Mutex<()>,
    #[cfg(test)]
    fault: Mutex<Option<StorageStep>>,
}
#[derive(Default)]
struct Inner {
    phase: QuiesceState,
    // Storage health is sticky, but only an explicit request gates normal turns.
    requested: bool,
    generation: u64,
    resumed_through: u64,
    cancelling: bool,
    shutting_down: bool,
    sealing: bool,
    revisions: BTreeMap<RunId, u64>,
    acks: BTreeMap<RunId, (u64, String, u64)>,
    // Prepared recovery has no running turn; its activation start gate is closed.
    unstarted: BTreeSet<RunId>,
    intents: BTreeMap<RunId, ResumeIntent>,
    persisted: BTreeSet<RunId>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StorageStep {
    Snapshot,
    Save,
    Flush,
}

/// Keep storage failure classes intact for answered-child retry policy.
#[derive(Debug)]
pub(crate) enum AdmissionError {
    Storage(storage::StorageError),
    Snapshot(storage::thread_index::SnapshotApiError),
    Invalid(String),
}
impl std::fmt::Display for AdmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Storage(error) => write!(f, "{error}"),
            Self::Snapshot(error) => {
                f.write_str(&crate::restore::provenance::save_failure_reason(error))
            }
            Self::Invalid(reason) => f.write_str(reason),
        }
    }
}
impl From<&str> for AdmissionError {
    fn from(reason: &str) -> Self {
        Self::Invalid(reason.into())
    }
}
impl From<String> for AdmissionError {
    fn from(reason: String) -> Self {
        Self::Invalid(reason)
    }
}
impl From<storage::StorageError> for AdmissionError {
    fn from(error: storage::StorageError) -> Self {
        Self::Storage(error)
    }
}
impl Default for Coordinator {
    fn default() -> Self {
        Self {
            inner: Mutex::default(),
            version: watch::channel(0).0,
            checkpoint_gate: Mutex::new(()),
            #[cfg(test)]
            fault: Mutex::default(),
        }
    }
}
impl Coordinator {
    pub(crate) fn storage_step(&self, step: StorageStep) -> Result<(), String> {
        #[cfg(test)]
        {
            let mut fault = self.fault.lock().unwrap();
            if *fault == Some(step) {
                *fault = None;
                return Err(format!("injected {step:?} failure"));
            }
        }
        let _ = step;
        Ok(())
    }
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn changed(&self) {
        self.version.send_modify(|n| *n = n.wrapping_add(1));
    }
    pub(crate) fn subscribe(&self) -> watch::Receiver<u64> {
        self.version.subscribe()
    }
    pub(crate) fn state(&self) -> QuiesceState {
        self.lock().phase.clone()
    }
    pub(crate) fn queued(&self, run: RunId, incarnation: &str) -> bool {
        self.lock().intents.get(&run).is_some_and(|intent| {
            intent.incarnation == incarnation && intent.wait == ResumeWait::AdmissionQueued
        })
    }
    pub(crate) fn provider_input(&self, run: RunId) -> Option<ProviderInput> {
        self.lock()
            .intents
            .get(&run)
            .and_then(|intent| intent.provider_input.clone())
    }
    pub(crate) fn active(&self, run: RunId) -> bool {
        self.lock()
            .intents
            .get(&run)
            .is_some_and(|intent| intent.active)
    }
    pub(crate) fn running(&self) -> bool {
        !self.lock().requested
    }
    pub(crate) fn fail(&self, reason: impl ToString) {
        let mut inner = self.lock();
        if !matches!(inner.phase, QuiesceState::Failed { .. }) {
            inner.phase = QuiesceState::Failed {
                reason: reason.to_string(),
            };
        }
        drop(inner);
        self.changed();
    }
    fn save(
        &self,
        store: Option<&RunStore>,
        intent: &ResumeIntent,
        expected: Option<String>,
        snapshot: Option<&storage::thread_index::SnapshotWrite>,
    ) -> Result<Option<storage::thread_index::SnapshotReceipt>, AdmissionError> {
        let Some(store) = store else {
            return if snapshot.is_some() {
                Err("restore storage is unavailable".into())
            } else {
                Ok(None)
            };
        };
        let step = |step| {
            self.storage_step(step)
                .map_err(|reason| AdmissionError::Storage(storage::StorageError::Io(reason)))
        };
        step(StorageStep::Save)?;
        let record = storage::resume_intent::ResumeIntentRecord {
            run_id: intent.run_id.to_string(),
            incarnation: intent.incarnation.clone(),
            payload: serde_json::to_string(intent)
                .map_err(|e| AdmissionError::Invalid(e.to_string()))?,
        };
        if let Some(snapshot) = snapshot {
            // All fallible flush work precedes consumption. The following writer
            // transaction durably commits both records, or neither of them.
            step(StorageStep::Flush)?;
            store.handle.flush_resume_intents()?;
            step(StorageStep::Snapshot)?;
            return store
                .handle
                .save_snapshot_bundle_with_resume_intent(snapshot, record, expected)
                .map(Some)
                .map_err(AdmissionError::Snapshot);
        }
        store.handle.save_resume_intent(record, expected)?;
        step(StorageStep::Flush)?;
        store
            .handle
            .flush_resume_intents()
            .map(|()| None)
            .map_err(AdmissionError::Storage)
    }
    /// Serialized with quiesce. Same incarnation or another live incarnation is
    /// never admitted twice. New input during quiesce remains a durable intent.
    pub(crate) fn admit(
        &self,
        mut intent: ResumeIntent,
        store: Option<&RunStore>,
        defer_registration: bool,
        snapshot: Option<&storage::thread_index::SnapshotWrite>,
    ) -> Result<(bool, Option<storage::thread_index::SnapshotReceipt>), AdmissionError> {
        let mut inner = self.lock();
        if inner
            .intents
            .get(&intent.run_id)
            .is_some_and(|old| old.active)
        {
            return Err("run incarnation already admitted".into());
        }
        let start = !inner.requested;
        if !start && defer_registration {
            intent.wait = ResumeWait::AdmissionQueued;
        }
        let previous = if let Some(old) = inner.intents.get(&intent.run_id) {
            Some(old.clone())
        } else if let Some(store) = store {
            let read = store
                .with_database(|db| db.resume_intent(&intent.run_id.to_string()))
                .map_err(AdmissionError::Storage)
                .and_then(|row| {
                    row.map(ResumeIntent::decode)
                        .transpose()
                        .map_err(AdmissionError::Invalid)
                });
            match read {
                Ok(previous) => previous,
                Err(reason) => {
                    inner.phase = QuiesceState::Failed {
                        reason: reason.to_string(),
                    };
                    self.changed();
                    return Err(reason);
                }
            }
        } else {
            None
        };
        let persisted =
            !inner.intents.contains_key(&intent.run_id) || inner.persisted.contains(&intent.run_id);
        let expected = previous
            .as_ref()
            .filter(|_| persisted)
            .map(|old| old.incarnation.clone());
        if let Some(old) = previous {
            // Re-registration changes authority/incarnation, never the already
            // sent prefix or an input no committed snapshot consumed. PR 06
            // decides which pending inputs to replay; admission cannot delete them.
            intent.inputs = old.inputs;
            if let Some(initial) = old.initial_input
                && intent.initial_input.as_ref() != Some(&initial)
                && !intent.inputs.iter().any(|input| input == &initial)
            {
                // A history-only registration did not consume this initial input.
                // Keep it queued until PR 06 actually delivers it to the context.
                intent.inputs.insert(0, initial);
            }
            intent.provider_input = old.provider_input;
            intent.snapshot = old.snapshot;
            intent.tool_intent = old.tool_intent;
            intent.boundary_pending = old.boundary_pending;
            intent.responses_replay = old.responses_replay;
            intent.compactions = old.compactions;
            intent.route_pins = old.route_pins;
        }
        let receipt = match self.save(store, &intent, expected, snapshot) {
            Ok(receipt) => receipt,
            Err(error) => {
                inner.phase = QuiesceState::Failed {
                    reason: error.to_string(),
                };
                self.changed();
                return Err(error);
            }
        };
        if store.is_some() {
            inner.persisted.insert(intent.run_id);
        }
        inner.intents.insert(intent.run_id, intent);
        self.changed();
        Ok((start, receipt))
    }
    /// Caller has fenced the entire component and has either retained its old
    /// durable intents or received the atomic activation ACK. Never write again.
    pub(crate) fn adopt(&self, intents: impl IntoIterator<Item = ResumeIntent>, persisted: bool) {
        self.adopt_kind(intents, persisted, false);
    }
    pub(crate) fn adopt_unstarted(&self, intents: impl IntoIterator<Item = ResumeIntent>) {
        self.adopt_kind(intents, true, true);
    }
    fn adopt_kind(
        &self,
        intents: impl IntoIterator<Item = ResumeIntent>,
        persisted: bool,
        unstarted: bool,
    ) {
        let mut inner = self.lock();
        for intent in intents {
            if persisted {
                inner.persisted.insert(intent.run_id);
            }
            if unstarted {
                inner.unstarted.insert(intent.run_id);
            } else {
                inner.unstarted.remove(&intent.run_id);
            }
            inner.acks.remove(&intent.run_id);
            inner.intents.insert(intent.run_id, intent);
            if inner.requested && inner.phase == QuiesceState::Checkpointed {
                inner.phase = QuiesceState::WaitingForSafePoints;
            }
        }
        self.changed();
    }
    /// Forget only the indexed old incarnation after a durable release ACK.
    pub(crate) fn forget_released(&self, run: RunId, incarnation: &str) {
        let mut inner = self.lock();
        if inner
            .intents
            .get(&run)
            .is_some_and(|intent| intent.incarnation == incarnation)
        {
            inner.intents.remove(&run);
            inner.persisted.remove(&run);
            self.changed();
        }
    }
    pub(crate) fn update(
        &self,
        run: RunId,
        incarnation: &str,
        store: Option<&RunStore>,
        change: impl FnOnce(&mut ResumeIntent),
    ) -> Result<(), String> {
        let mut inner = self.lock();
        let Some(old) = inner
            .intents
            .get(&run)
            .filter(|old| old.incarnation == incarnation)
        else {
            return Ok(());
        };
        let mut intent = old.clone();
        change(&mut intent);
        if *old == intent {
            return Ok(());
        }
        let expected = inner
            .persisted
            .contains(&run)
            .then(|| incarnation.to_string());
        if let Err(error) = self.save(store, &intent, expected, None) {
            inner.phase = QuiesceState::Failed {
                reason: error.to_string(),
            };
            self.changed();
            return Err(error.to_string());
        }
        if store.is_some() {
            inner.persisted.insert(run);
        }
        *inner.revisions.entry(run).or_default() += 1;
        inner.intents.insert(run, intent);
        if inner.requested && inner.phase == QuiesceState::Checkpointed {
            inner.phase = QuiesceState::WaitingForSafePoints;
        }
        self.changed();
        Ok(())
    }
    pub(crate) fn retire(
        &self,
        run: RunId,
        incarnation: &str,
        store: Option<&RunStore>,
        reason: StopReason,
    ) -> Result<(), String> {
        self.finish_intent_update(run, incarnation, store, reason, true)
    }
    pub(crate) fn stop_unregistered(
        &self,
        run: RunId,
        incarnation: &str,
        store: Option<&RunStore>,
        reason: StopReason,
    ) -> Result<(), String> {
        self.finish_intent_update(run, incarnation, store, reason, false)
    }
    fn finish_intent_update(
        &self,
        run: RunId,
        incarnation: &str,
        store: Option<&RunStore>,
        reason: StopReason,
        completed: bool,
    ) -> Result<(), String> {
        use storage::resume_intent::ResumeIntentWriteOutcome;
        let mut inner = self.lock();
        let Some(old) = inner
            .intents
            .get(&run)
            .filter(|old| old.incarnation == incarnation)
        else {
            return Ok(());
        };
        let mut intent = old.clone();
        if completed {
            intent.active = false;
            if reason == StopReason::Cancelled {
                intent.resume_requested = false;
            }
            if intent.stop_reason.is_none() || reason == StopReason::Cancelled {
                intent.stop_reason = Some(reason);
            }
        } else {
            if intent.stop_reason != Some(StopReason::CliDisconnected)
                || reason == StopReason::Cancelled
            {
                intent.stop_reason = Some(reason);
            }
            intent.resume_requested = false;
            if intent.wait == ResumeWait::AdmissionQueued {
                intent.active = false;
            }
        }
        if *old == intent {
            return Ok(());
        }
        let saved = (|| {
            if let Some(store) = store
                && inner.persisted.contains(&run)
            {
                self.storage_step(StorageStep::Save)?;
                let record = storage::resume_intent::ResumeIntentRecord {
                    run_id: run.to_string(),
                    incarnation: incarnation.into(),
                    payload: serde_json::to_string(&intent).map_err(|error| error.to_string())?,
                };
                let outcome = if completed {
                    store.handle.retire_resume_intent(record)
                } else {
                    store.handle.stop_unregistered_resume_intent(record)
                }
                .map_err(|error| error.to_string())?;
                self.storage_step(StorageStep::Flush)?;
                store
                    .handle
                    .flush_resume_intents()
                    .map_err(|error| error.to_string())?;
                Ok(outcome)
            } else {
                // With no run store this is local-only. A newly attached store
                // still uses the strict initial-insert CAS until first persisted.
                self.save(store, &intent, None, None)
                    .map(|_| ResumeIntentWriteOutcome::Saved)
                    .map_err(|error| error.to_string())
            }
        })();
        match saved {
            Ok(ResumeIntentWriteOutcome::Replaced { current }) => {
                tracing::debug!(run = %run, old = incarnation, current = %current, "resume intent superseded by another owner");
                // A successor already owns the durable row. Remove only this
                // obsolete local copy so later cleanup cannot update it again.
                inner.intents.remove(&run);
                inner.persisted.remove(&run);
            }
            Ok(ResumeIntentWriteOutcome::Saved) => {
                if store.is_some() {
                    inner.persisted.insert(run);
                }
                inner.intents.insert(run, intent);
            }
            Err(error) => {
                inner.phase = QuiesceState::Failed {
                    reason: error.clone(),
                };
                self.changed();
                return Err(error);
            }
        }
        self.changed();
        Ok(())
    }

    pub(crate) fn begin(&self) -> Result<u64, String> {
        let mut inner = self.lock();
        if let QuiesceState::Failed { reason } = &inner.phase {
            return Err(reason.clone());
        }
        if inner.shutting_down || inner.cancelling {
            return Err("runtime is not accepting restart requests".into());
        }
        if !inner.requested {
            inner.generation = inner
                .generation
                .checked_add(1)
                .ok_or("restart generation exhausted")?;
            inner.acks.clear();
            inner.requested = true;
            inner.phase = QuiesceState::WaitingForSafePoints;
            self.changed();
        }
        Ok(inner.generation)
    }
    pub(crate) fn complete(
        &self,
        store: Option<&RunStore>,
        unstarted: &BTreeSet<RunId>,
    ) -> Result<bool, String> {
        let mut inner = self.lock();
        match &inner.phase {
            QuiesceState::Failed { reason } => return Err(reason.clone()),
            QuiesceState::Checkpointed => return Ok(true),
            QuiesceState::WaitingForSafePoints => {}
            _ => return Ok(false),
        }
        if inner.intents.values().any(|intent| {
            intent.active
                && !unstarted.contains(&intent.run_id)
                && intent.wait != ResumeWait::AdmissionQueued
                && !inner.acks.get(&intent.run_id).is_some_and(
                    |(generation, incarnation, revision)| {
                        *generation == inner.generation
                            && *incarnation == intent.incarnation
                            && *revision == *inner.revisions.get(&intent.run_id).unwrap_or(&0)
                    },
                )
        }) {
            return Ok(false);
        }
        let flushed = self.storage_step(StorageStep::Flush).and_then(|()| {
            store
                .ok_or_else(|| "safe-point quiesce requires a run store".to_string())
                .and_then(|store| {
                    store
                        .handle
                        .flush_resume_intents()
                        .map_err(|e| e.to_string())
                })
        });
        if let Err(error) = flushed {
            inner.phase = QuiesceState::Failed {
                reason: error.to_string(),
            };
            self.changed();
            return Err(error);
        }
        inner.phase = QuiesceState::Checkpointed;
        self.changed();
        Ok(true)
    }
}
