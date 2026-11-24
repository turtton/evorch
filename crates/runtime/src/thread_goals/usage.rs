//! Durable counters and display checkpoints are independent of semantic snapshots.
use super::*;
use std::sync::Arc;

const REQUEST_CHECKPOINT: u32 = 16;
// Sequential-shaped samples have p90 342,668, but request attribution is not
// confirmed. Retain the planned 8M fallback (also above 16 times that estimate).
const TOKEN_CHECKPOINT: u64 = 8_000_000;
const MAX_UNSAVED_REQUESTS: u32 = 15;

pub(super) struct UsageState {
    pub(super) identity: Arc<()>,
    pub(super) acked: ThreadGoalUsage,
    pub(super) checkpoint: ThreadGoalUsage,
    pub(super) write_failure: Option<Arc<()>>,
    pub(super) read_failure: Option<Arc<()>>,
}

impl UsageState {
    pub(super) fn new(usage: &ThreadGoalUsage, read_failed: bool) -> Self {
        Self {
            identity: Arc::new(()),
            acked: usage.clone(),
            checkpoint: usage.clone(),
            write_failure: None,
            read_failure: read_failed.then(|| Arc::new(())),
        }
    }
}

fn max_usage(target: &mut ThreadGoalUsage, saved: &ThreadGoalUsage) {
    target.model_requests = target.model_requests.max(saved.model_requests);
    target.input_tokens = target.input_tokens.max(saved.input_tokens);
    target.output_tokens = target.output_tokens.max(saved.output_tokens);
}

fn covers(saved: &ThreadGoalUsage, current: &ThreadGoalUsage) -> bool {
    saved.model_requests >= current.model_requests
        && saved.input_tokens >= current.input_tokens
        && saved.output_tokens >= current.output_tokens
}

struct Save {
    id: String,
    identity: Arc<()>,
    thread: String,
    root: String,
    usage: ThreadGoalUsage,
    recovery: Option<Arc<()>>,
}

impl Save {
    fn capture(entry: &GoalEntry) -> Self {
        Self {
            id: entry.snapshot.goal_id.clone(),
            identity: entry.persistence.identity.clone(),
            thread: entry.snapshot.thread_id.clone(),
            root: entry.snapshot.root_run_id.clone(),
            usage: entry.snapshot.usage.clone(),
            recovery: entry.persistence.write_failure.clone(),
        }
    }
}

fn same_entry<'a>(
    goals: &'a mut ThreadGoals,
    id: &str,
    identity: &Arc<()>,
) -> Option<&'a mut GoalEntry> {
    goals.goals.values_mut().find(|entry| {
        entry.snapshot.goal_id == id && Arc::ptr_eq(&entry.persistence.identity, identity)
    })
}

impl AgentRuntime {
    /// Called before taking any publication/authority lock. A bad row must not
    /// erase a semantic goal or prevent unrelated goals from being restored.
    pub(super) fn restore_goal_usage(&self, snapshot: &mut ThreadGoalSnapshot) -> bool {
        let Some(store) = self.shared.run_store.get() else {
            tracing::warn!(
                "restoring goal without run store: legacy full-snapshot mode; durable usage guarantees do not apply and saved display counters may lag"
            );
            return false;
        };
        match store.with_database(|db| db.thread_goal_usage(&snapshot.goal_id)) {
            Ok(Some(saved)) => {
                max_usage(&mut snapshot.usage, &saved);
                false
            }
            Ok(None) => false,
            Err(error) => {
                tracing::warn!(%error, "cannot read durable goal usage; retaining goal with request latch");
                true
            }
        }
    }

    fn save_goal_usage(&self, save: Save) -> Result<(), String> {
        let store = self.shared.run_store.get().expect("durable usage mode");
        // No goals/runs/intents mutex or plan authority gate is held here.
        let result =
            store
                .handle
                .upsert_thread_goal_usage(&save.id, &save.thread, &save.root, &save.usage);
        let mut goals = self.goal_lock();
        if let Some(entry) = same_entry(&mut goals, &save.id, &save.identity) {
            match &result {
                Ok(()) => {
                    max_usage(&mut entry.persistence.acked, &save.usage);
                    if let (Some(recovery), Some(failure)) =
                        (&save.recovery, &entry.persistence.write_failure)
                        && Arc::ptr_eq(recovery, failure)
                        && covers(&entry.persistence.acked, &entry.snapshot.usage)
                    {
                        entry.persistence.write_failure = None;
                    }
                }
                Err(_) => entry.persistence.write_failure = Some(Arc::new(())),
            }
        }
        result.map_err(|e| format!("Cannot persist thread goal usage: {e}"))
    }

    fn recover_goal_usage_read(
        &self,
        id: String,
        identity: Arc<()>,
        generation: Arc<()>,
    ) -> Result<(), String> {
        let store = self.shared.run_store.get().expect("durable usage mode");
        let result = store.with_database(|db| db.thread_goal_usage(&id));
        let mut goals = self.goal_lock();
        if let Some(entry) = same_entry(&mut goals, &id, &identity) {
            match &result {
                Ok(saved) => {
                    if let Some(saved) = saved {
                        max_usage(&mut entry.snapshot.usage, saved);
                        max_usage(&mut entry.persistence.acked, saved);
                    }
                    if entry
                        .persistence
                        .read_failure
                        .as_ref()
                        .is_some_and(|failure| Arc::ptr_eq(failure, &generation))
                    {
                        entry.persistence.read_failure = None;
                    }
                }
                Err(_) => entry.persistence.read_failure = Some(Arc::new(())),
            }
        }
        result
            .map(|_| ())
            .map_err(|e| format!("Cannot read thread goal usage: {e}"))
    }

    fn publish_usage_change(&self, entry: &mut GoalEntry, semantic: bool) {
        let current = &entry.snapshot.usage;
        let base = &entry.persistence.checkpoint;
        let checkpoint = current.model_requests.saturating_sub(base.model_requests)
            >= REQUEST_CHECKPOINT
            || current
                .input_tokens
                .saturating_sub(base.input_tokens)
                .saturating_add(current.output_tokens.saturating_sub(base.output_tokens))
                >= TOKEN_CHECKPOINT;
        if self.shared.run_store.get().is_none() || semantic || checkpoint {
            if checkpoint {
                entry.persistence.checkpoint = current.clone();
            }
            self.publish_thread_goal(&entry.snapshot);
        }
    }

    fn usage_owner(&self, run: RunId, purpose: crate::RunPurpose) -> RunId {
        match purpose {
            crate::RunPurpose::ThreadGoalReview { root_run_id, .. } => root_run_id,
            _ => self.goal_owner_for_run(run).unwrap_or(run),
        }
    }

    pub(crate) fn goal_model_request(
        &self,
        run: RunId,
        purpose: crate::RunPurpose,
        token_limit: Option<u64>,
    ) -> Result<bool, String> {
        enum Work {
            Save(Save),
            Read(String, Arc<()>, Arc<()>),
        }
        loop {
            let root = self.usage_owner(run, purpose);
            let work = {
                let mut goals = self.goal_lock();
                let Some(thread) = goals.roots.get(&root).cloned() else {
                    return Ok(true);
                };
                let Some(entry) = goals.goals.get_mut(&thread) else {
                    return Ok(true);
                };
                if entry.snapshot.phase == ThreadGoalPhase::Complete {
                    return Ok(true);
                }
                let durable = self.shared.run_store.get().is_some();
                if let Some(generation) = &entry.persistence.read_failure {
                    Work::Read(
                        entry.snapshot.goal_id.clone(),
                        entry.persistence.identity.clone(),
                        generation.clone(),
                    )
                } else if entry.persistence.write_failure.is_some() {
                    Work::Save(Save::capture(entry))
                } else {
                    let before = (
                        entry.snapshot.max_tokens,
                        entry.snapshot.phase,
                        entry.snapshot.reason.clone(),
                    );
                    if run == root
                        && purpose == crate::RunPurpose::General
                        && let Some(limit) = token_limit
                    {
                        entry.snapshot.max_tokens = Some(
                            entry
                                .snapshot
                                .max_tokens
                                .map_or(limit, |previous| previous.min(limit)),
                        );
                    }
                    let used = entry
                        .snapshot
                        .usage
                        .input_tokens
                        .saturating_add(entry.snapshot.usage.output_tokens);
                    if entry.snapshot.max_tokens.is_some_and(|limit| used >= limit) {
                        block_budget(entry);
                        let semantic = before
                            != (
                                entry.snapshot.max_tokens,
                                entry.snapshot.phase,
                                entry.snapshot.reason.clone(),
                            );
                        self.publish_usage_change(entry, semantic);
                        return Ok(false);
                    }
                    let semantic = before.0 != entry.snapshot.max_tokens;
                    if durable
                        && entry
                            .snapshot
                            .usage
                            .model_requests
                            .saturating_sub(entry.persistence.acked.model_requests)
                            >= MAX_UNSAVED_REQUESTS
                    {
                        // A setting change remains immediate even while the counter
                        // boundary is waiting for its commit.
                        if semantic {
                            self.publish_usage_change(entry, true);
                        }
                        Work::Save(Save::capture(entry))
                    } else {
                        entry.snapshot.usage.model_requests =
                            entry.snapshot.usage.model_requests.saturating_add(1);
                        self.publish_usage_change(entry, semantic);
                        return Ok(true);
                    }
                }
            };
            match work {
                Work::Save(save) => self.save_goal_usage(save)?,
                Work::Read(id, identity, generation) => {
                    self.recover_goal_usage_read(id, identity, generation)?
                }
            }
            // Re-resolve ownership, identity, latches, budget and the shared
            // unsaved count after every wait; another save may still be needed.
        }
    }

    pub(crate) fn goal_usage(
        &self,
        run: RunId,
        purpose: crate::RunPurpose,
        usage: providers::Usage,
    ) -> Result<(), String> {
        let root = self.usage_owner(run, purpose);
        let save = {
            let mut goals = self.goal_lock();
            let Some(thread) = goals.roots.get(&root).cloned() else {
                return Ok(());
            };
            let Some(entry) = goals.goals.get_mut(&thread) else {
                return Ok(());
            };
            if entry.snapshot.phase == ThreadGoalPhase::Complete {
                return Ok(());
            }
            let before = (entry.snapshot.phase, entry.snapshot.reason.clone());
            entry.snapshot.usage.input_tokens = entry
                .snapshot
                .usage
                .input_tokens
                .saturating_add(usage.input_tokens);
            entry.snapshot.usage.output_tokens = entry
                .snapshot
                .usage
                .output_tokens
                .saturating_add(usage.output_tokens);
            let total = entry
                .snapshot
                .usage
                .input_tokens
                .saturating_add(entry.snapshot.usage.output_tokens);
            if entry.snapshot.max_tokens.is_some_and(|limit| total > limit) {
                block_budget(entry);
            }
            let semantic = before != (entry.snapshot.phase, entry.snapshot.reason.clone());
            self.publish_usage_change(entry, semantic);
            if self.shared.run_store.get().is_none() {
                return Ok(());
            }
            if entry.persistence.read_failure.is_some() {
                return Err(
                    "Cannot read thread goal usage: recovery required before saving".into(),
                );
            }
            // An ordinary response ACK cannot clear an earlier write failure.
            let mut save = Save::capture(entry);
            save.recovery = None;
            save
        };
        self.save_goal_usage(save)
    }
}

fn block_budget(entry: &mut GoalEntry) {
    entry.snapshot.phase = ThreadGoalPhase::Blocked;
    entry.snapshot.reason = Some(format!(
        "Goal cumulative token budget exhausted; automatic checking is stopped.{RECOVERY_HINT}"
    ));
}
