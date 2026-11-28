//! Plan handlers registered as the `plan_read`/`plan_update` catalog entries. No goal or wake integration.
//! Lock order: per-thread authority gate -> registry mutation guard -> short goal/runs
//! reads. DB round trips retain only the authority guards. No projection cache exists.
pub(crate) mod notice;
#[cfg(test)]
mod tests;
pub(crate) mod tools;
/// Name of the retired todo tool, outside every catalog.
pub(crate) const RETIRED_TODO_TOOL: &str = "todo_write";
/// Fixed answer to a call of the retired tool from a saved history; it changes nothing.
pub(crate) const RETIRED_TODO_WRITE: &str = "`todo_write`は廃止されました。現在の計画には`plan_read`と`plan_update`を使用してください。旧Todoデータは引き継がれず、この呼出しによる変更はありません。";
/// Shared eligibility predicate.
pub(crate) fn conversation_root(state: &crate::agent_loop::LoopState) -> bool {
    state.task.parent.is_none()
        && state.run_config().conversation
        && state.run_config().purpose == crate::RunPurpose::General
        && matches!(
            state.run_role(),
            crate::Role::Worker | crate::Role::Orchestrator
        )
}
use crate::{AgentRuntime, RunId};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
};
use storage::{StorageError, StorageHandle, conversation_plan::*};

#[derive(Default)]
pub(crate) struct AuthorityGates(Mutex<HashMap<String, Weak<Mutex<()>>>>);
impl AuthorityGates {
    pub(crate) fn get(&self, thread: &str) -> Arc<Mutex<()>> {
        let mut gates = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        gates.retain(|_, gate| gate.strong_count() != 0);
        if let Some(gate) = gates.get(thread).and_then(Weak::upgrade) {
            return gate;
        }
        let gate = Arc::new(Mutex::new(()));
        gates.insert(thread.into(), Arc::downgrade(&gate));
        gate
    }
}

/// Captured from the original execution, not looked up by reusable RunId later.
pub(crate) struct Caller<'a> {
    pub(crate) run: RunId,
    pub(crate) mailbox: &'a Arc<crate::mailbox::RunMailbox>,
}
impl AgentRuntime {
    pub(crate) fn plan_store(&self) -> Result<&StorageHandle, StorageError> {
        self.shared
            .run_store
            .get()
            .map(|s| &s.handle)
            .ok_or(StorageError::WriterClosed)
    }
    fn with_plan_authority<T>(
        &self,
        caller: &Caller<'_>,
        action: impl FnOnce(&StorageHandle, &str) -> Result<T, String>,
    ) -> Result<T, String> {
        let thread = self
            .goal_thread(caller.run)
            .ok_or("binding_conflict: no current conversation root")?;
        let gate = self.shared.plan_authority.get(&thread);
        let _gate = gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.goal_thread(caller.run).as_deref() != Some(&thread) {
            return Err("binding_conflict: root was replaced".into());
        }
        {
            let runs = self
                .shared
                .runs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let run = runs
                .get(&caller.run)
                .ok_or("binding_conflict: run is no longer live")?;
            if run.parent.is_some()
                || !run.config.conversation
                || run.config.purpose != crate::RunPurpose::General
                || !matches!(run.role, crate::Role::Worker | crate::Role::Orchestrator)
                || !Arc::ptr_eq(&run.mailbox, caller.mailbox)
            {
                return Err("binding_conflict: not the current conversation root execution".into());
            }
        }
        // The DB transaction checks tombstones/binding; no goal/runs lock is held here.
        action(self.plan_store().map_err(storage_error)?, &thread)
    }
    pub(crate) fn apply_plan(
        &self,
        caller: &Caller<'_>,
        expected_revision: u64,
        operations: Vec<PlanOperation>,
    ) -> Result<PlanApplied, String> {
        self.with_plan_authority(caller, |store, thread| {
            let applied = store
                .update_conversation_plan(PlanUpdate {
                    thread_id: thread.into(),
                    expected_revision,
                    source_run_id: Some(caller.run.to_string()),
                    operations,
                })
                .map_err(storage_error)?;
            self.notify_plan(&applied);
            Ok(applied)
        })
    }
    fn notify_plan(&self, applied: &PlanApplied) {
        // Re-emit the saved event identity/metadata, never store it a second time.
        if let Some(event) = &applied.event {
            self.shared.bus.emit(event.event.clone());
        }
    }
    pub(crate) fn reconcile_plan_or_report(&self, caller: &Caller<'_>) -> bool {
        if self.goal_thread(caller.run).is_none() || !self.has_run_store() {
            return false;
        }
        match self.with_plan_authority(caller, |store, thread| {
            let applied = store
                .reconcile_conversation_plan(thread, Some(&caller.run.to_string()))
                .map_err(storage_error)?;
            self.notify_plan(&applied);
            Ok(applied.changed)
        }) {
            Ok(changed) => changed,
            Err(reason) => {
                self.shared
                    .bus
                    .emit(event_bus::Event::new(event_bus::DiagnosticEvent {
                        source: "conversation_plan".into(),
                        severity: event_bus::DiagnosticSeverity::Warning,
                        code:
                            event_bus::event::diagnostic_codes::CONVERSATION_PLAN_RECONCILE_FAILED
                                .into(),
                        detail: reason,
                        run_id: Some(caller.run.to_string()),
                        thread_id: None,
                        call_id: None,
                    }));
                false
            }
        }
    }
    /// Read the authoritative projection; no event replay or runtime cache.
    pub fn conversation_plan_summary(&self, thread: &str) -> Result<PlanSummary, StorageError> {
        self.plan_store()?.conversation_plan_summary(thread)
    }
    pub fn conversation_plan_tree(
        &self,
        thread: &str,
        cursor: Option<&str>,
        limit: Option<u32>,
    ) -> Result<PlanTreePage, StorageError> {
        self.plan_store()?
            .conversation_plan_tree(thread, cursor, limit)
    }
    pub fn conversation_plan_task(
        &self,
        thread: &str,
        task: &str,
    ) -> Result<PlanTaskRead, StorageError> {
        self.plan_store()?.conversation_plan_task(thread, task)
    }
    pub fn conversation_plan_history(
        &self,
        thread: &str,
        after: Option<u64>,
        limit: Option<u32>,
    ) -> Result<PlanHistoryPage, StorageError> {
        self.plan_store()?
            .conversation_plan_history(thread, after, limit)
    }
}

fn storage_error(error: StorageError) -> String {
    match error {
        StorageError::StaleRevision {
            kind: "conversation_plan",
            ..
        } => "stale_revision: re-read the plan before retrying".into(),
        StorageError::ConversationPlan(PlanRejection::StaleCursor { .. }) => {
            "stale_cursor: plan changed; restart paging".into()
        }
        StorageError::ConversationPlan(PlanRejection::InvalidCursor) => {
            "invalid_request: invalid plan cursor".into()
        }
        StorageError::ConversationPlan(reason) => {
            // Rejections are bounded below; never return raw DB errors or paths.
            let detail = secret_guard::SecretRedactor::from_env()
                .redact(&reason.to_string())
                .text;
            format!(
                "plan_rejected: {}",
                detail.chars().take(256).collect::<String>()
            )
        }
        StorageError::Tombstoned { .. } => "thread_deleted: this conversation was deleted".into(),
        _ => "storage_unavailable: update outcome may be unknown; re-read before retrying".into(),
    }
}

impl crate::agent_loop::LoopState {
    /// A turn that ended without a blocking question pending ended this root's
    /// work period: mark unfinished tasks. A change reaches the model only through
    /// `plan_read`, never as a request of its own.
    pub(crate) fn reconcile_finished_turn(&self) {
        let Some(runtime) = self.runtime() else {
            return;
        };
        if runtime.has_pending_user_questions(self.caller_run_id()) {
            return;
        }
        self.reconcile_root_plan();
    }
    pub(crate) fn reconcile_root_plan(&self) {
        if let Some(runtime) = self.runtime() {
            runtime.reconcile_plan_or_report(&Caller {
                run: self.caller_run_id(),
                mailbox: &self.task.mailbox,
            });
        }
    }
}
