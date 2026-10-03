use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};

use crate::{
    AgentMessageEvent, CompactionEvent, Event, EventKind, LifecycleEvent, MessageEvent,
    ProviderEvent, ToolEvent,
};

pub type MutationCheck = Arc<dyn Fn() -> bool + Send + Sync>;
pub trait MutationGuard: Send {}
impl<T: Send> MutationGuard for T {}
pub type MutationGuardCheck = Arc<dyn Fn() -> Option<Box<dyn MutationGuard>> + Send + Sync>;

/// A nonblocking acquisition must distinguish contention from revoked authority.
pub enum MutationGuardAttempt {
    Acquired(Box<dyn MutationGuard>),
    Busy,
    Rejected,
}
pub type MutationGuardTryCheck = Arc<dyn Fn() -> MutationGuardAttempt + Send + Sync>;

#[derive(Debug, PartialEq, Eq)]
pub enum MutationBatchError {
    Busy,
    Poisoned,
}

enum Fence {
    Check(MutationCheck),
    Guard(MutationGuardCheck),
    NonblockingGuard {
        blocking: MutationGuardCheck,
        attempt: MutationGuardTryCheck,
    },
}

#[derive(Clone, Default)]
pub struct MutationValidator(Arc<MutationFences>);

impl MutationValidator {
    pub fn accepts(&self, event: &Event) -> bool {
        self.0.accepts(event)
    }

    pub fn acquire(&self, event: &Event) -> Option<Vec<Box<dyn MutationGuard>>> {
        self.0.acquire(event)
    }

    /// Consume a batch immediately only if none of its runs need a guard.
    /// Guard callbacks are never invoked here. A busy registry also defers the
    /// whole batch to its worker, preserving order and avoiding a GUI lock wait.
    pub fn try_check_batch(&self, events: Vec<Event>) -> Result<Vec<Event>, Vec<Event>> {
        let Ok(runs) = self.0.runs.try_read() else {
            return Err(events);
        };
        if events.iter().any(|event| {
            !accepts_runs(event, |run| {
                !matches!(
                    runs.get(run),
                    Some(Fence::Guard(_) | Fence::NonblockingGuard { .. })
                )
            })
        }) {
            return Err(events);
        }
        Ok(events
            .into_iter()
            .filter(|event| {
                accepts_runs(event, |run| match runs.get(run) {
                    Some(Fence::Check(check)) => check(),
                    None => true,
                    Some(Fence::Guard(_) | Fence::NonblockingGuard { .. }) => {
                        unreachable!("guarded events were deferred")
                    }
                })
            })
            .collect())
    }

    /// Acquire nonblocking guards once per run. Contention releases every guard
    /// already acquired and returns Busy, so a writer can finish before retry.
    /// Legacy blocking guards use the original single-event scope: never retain
    /// an earlier event's read transaction while acquiring a later event's guard.
    /// Keep the result alive through consumption, including `recheck_batch`.
    pub fn acquire_batch(
        &self,
        events: &[Event],
    ) -> Result<MutationBatchGuard, MutationBatchError> {
        let runs = self
            .0
            .runs
            .read()
            .map_err(|_| MutationBatchError::Poisoned)?;
        let legacy = events.iter().any(|event| {
            !accepts_runs(event, |run| !matches!(runs.get(run), Some(Fence::Guard(_))))
        });
        let events = if legacy {
            &events[..events.len().min(1)]
        } else {
            events
        };
        let mut seen = BTreeSet::new();
        let mut run_ids = Vec::new();
        for event in events {
            accepts_runs(event, |run| {
                if seen.insert(run.to_owned()) {
                    run_ids.push(run.to_owned());
                }
                true
            });
        }
        let mut checked = BTreeMap::new();
        let mut guards = Vec::new();
        // In a mixed event, acquire legacy callbacks before holding any of the
        // nonblocking guards. Multiple legacy callbacks retain their previous
        // per-event semantics; runtime ownership always supplies try callbacks.
        for run in &run_ids {
            if let Some(Fence::Guard(check)) = runs.get(run) {
                let accepted = match check() {
                    Some(guard) => {
                        guards.push(guard);
                        true
                    }
                    None => false,
                };
                checked.insert(run.clone(), accepted);
            }
        }
        for run in &run_ids {
            let accepted = match runs.get(run) {
                Some(Fence::NonblockingGuard { attempt, .. }) => match attempt() {
                    MutationGuardAttempt::Acquired(guard) => {
                        guards.push(guard);
                        true
                    }
                    MutationGuardAttempt::Busy => return Err(MutationBatchError::Busy),
                    MutationGuardAttempt::Rejected => false,
                },
                Some(Fence::Check(check)) => check(),
                Some(Fence::Guard(_)) => continue,
                None => true,
            };
            checked.insert(run.clone(), accepted);
        }
        let accepted = events
            .iter()
            .map(|event| accepts_runs(event, |run| checked[run]))
            .collect();
        Ok(MutationBatchGuard {
            _guards: guards,
            accepted,
            revision: runs.len(),
        })
    }

    /// Finish consuming a guarded batch without acquiring guard callbacks again.
    ///
    /// The caller must still hold the batch's guards. Legacy check-only callbacks
    /// are rechecked synchronously, so those callbacks must be fast for GUI use.
    /// A concurrently registered fence or busy registry requires revalidation;
    /// return the untouched events rather than accepting an unguarded mutation.
    pub fn recheck_batch(
        &self,
        events: Vec<Event>,
        revision: usize,
    ) -> Result<Vec<Event>, Vec<Event>> {
        let Ok(runs) = self.0.runs.try_read() else {
            return Err(events);
        };
        if runs.len() != revision {
            return Err(events);
        }
        Ok(events
            .into_iter()
            .filter(|event| {
                accepts_runs(event, |run| match runs.get(run) {
                    Some(Fence::Check(check)) => check(),
                    None | Some(Fence::Guard(_) | Fence::NonblockingGuard { .. }) => true,
                })
            })
            .collect())
    }

    pub(crate) fn new(fences: Arc<MutationFences>) -> Self {
        Self(fences)
    }
}

/// Guards for the accepted entries of one event batch.
/// Dropping this value releases the authority protected by those guards.
pub struct MutationBatchGuard {
    _guards: Vec<Box<dyn MutationGuard>>,
    accepted: Vec<bool>,
    revision: usize,
}

impl MutationBatchGuard {
    pub fn accepted(&self) -> &[bool] {
        &self.accepted
    }

    pub fn revision(&self) -> usize {
        self.revision
    }
}

#[derive(Default)]
pub(crate) struct MutationFences {
    runs: RwLock<BTreeMap<String, Fence>>,
}

impl MutationFences {
    pub(crate) fn register(&self, run: String, check: MutationCheck) -> bool {
        self.insert(run, Fence::Check(check))
    }

    pub(crate) fn register_guard(&self, run: String, check: MutationGuardCheck) -> bool {
        self.insert(run, Fence::Guard(check))
    }

    pub(crate) fn register_nonblocking_guard(
        &self,
        run: String,
        blocking: MutationGuardCheck,
        attempt: MutationGuardTryCheck,
    ) -> bool {
        self.insert(run, Fence::NonblockingGuard { blocking, attempt })
    }

    fn insert(&self, run: String, fence: Fence) -> bool {
        let Ok(mut runs) = self.runs.write() else {
            return false;
        };
        match runs.entry(run) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(fence);
                true
            }
            std::collections::btree_map::Entry::Occupied(_) => false,
        }
    }

    pub(crate) fn accepts(&self, event: &Event) -> bool {
        self.acquire(event).is_some()
    }

    pub(crate) fn acquire(&self, event: &Event) -> Option<Vec<Box<dyn MutationGuard>>> {
        let runs = self.runs.read().ok()?;
        let mut guards = Vec::new();
        let accepted = accepts_runs(event, |run| acquire_run(&runs, run, &mut guards));
        accepted.then_some(guards)
    }
}

fn acquire_run(
    runs: &BTreeMap<String, Fence>,
    run: &str,
    guards: &mut Vec<Box<dyn MutationGuard>>,
) -> bool {
    match runs.get(run) {
        None => true,
        Some(Fence::Check(check)) => check(),
        Some(
            Fence::Guard(check)
            | Fence::NonblockingGuard {
                blocking: check, ..
            },
        ) => match check() {
            Some(guard) => {
                guards.push(guard);
                true
            }
            None => false,
        },
    }
}

fn accepts_runs(event: &Event, mut accepts: impl FnMut(&str) -> bool) -> bool {
    match &event.kind {
        EventKind::Lifecycle(event) => match event {
            LifecycleEvent::WorkspaceWaitChanged { run_id, .. }
            | LifecycleEvent::RunProgress { run_id, .. }
            | LifecycleEvent::AgentRunStarted { run_id, .. }
            | LifecycleEvent::TaskPromptPublished { run_id, .. }
            | LifecycleEvent::AgentRunRestored { run_id, .. }
            | LifecycleEvent::AgentRunStateChanged { run_id, .. }
            | LifecycleEvent::TurnCompleted { run_id, .. }
            | LifecycleEvent::EscalationProposed { run_id, .. } => accepts(run_id),
            LifecycleEvent::BackgroundTaskStarted { task_id }
            | LifecycleEvent::BackgroundTaskCompleted { task_id }
            | LifecycleEvent::BackgroundTaskCancelled { task_id } => accepts(task_id),
            LifecycleEvent::Started { session_id }
            | LifecycleEvent::Completed { session_id }
            | LifecycleEvent::Failed { session_id, .. }
            | LifecycleEvent::Delegated { session_id, .. } => accepts(session_id),
            LifecycleEvent::EscalationRequested {
                source_run_id,
                new_run_id,
                ..
            } => accepts(source_run_id) && accepts(new_run_id),
            LifecycleEvent::RoutingDecision { .. } => true,
        },
        EventKind::Message(
            MessageEvent::MessageDelta { run_id, .. } | MessageEvent::ReasoningDelta { run_id, .. },
        )
        | EventKind::Tool(
            ToolEvent::ToolStarted { run_id, .. } | ToolEvent::ToolCompleted { run_id, .. },
        )
        | EventKind::Provider(
            ProviderEvent::RequestStarted { run_id, .. }
            | ProviderEvent::FirstTokenObserved { run_id, .. }
            | ProviderEvent::RequestCompleted { run_id, .. }
            | ProviderEvent::CacheReuseObserved { run_id, .. }
            | ProviderEvent::RequestFailed { run_id, .. },
        ) => run_id.as_deref().is_none_or(accepts),
        EventKind::Message(
            MessageEvent::FinalResultPublished { run_id, .. }
            | MessageEvent::MessageCompleted { run_id, .. },
        ) => accepts(run_id),
        EventKind::Tool(ToolEvent::UserQuestionUpdated { question }) => accepts(&question.run_id),
        EventKind::Tool(
            ToolEvent::ApprovalRequested { call_id, .. }
            | ToolEvent::ApprovalResolved { call_id, .. }
            | ToolEvent::ExecutionDenied { call_id, .. },
        ) => call_id.split_once(':').is_none_or(|(run, _)| accepts(run)),
        EventKind::Provider(ProviderEvent::FallbackTriggered { session_id, .. }) => {
            accepts(session_id)
        }
        EventKind::AgentMessage(AgentMessageEvent::Delivered { message, .. }) => {
            accepts(&message.sender_run_id) && accepts(&message.recipient_run_id)
        }
        EventKind::Compaction(CompactionEvent::Compacted { run_id, .. }) => accepts(run_id),
        EventKind::Snapshot(event) => accepts(&event.run_id),
        EventKind::Ledger(crate::LedgerEvent::RunLedgerAppended { run_id, .. }) => accepts(run_id),
        EventKind::Diagnostic(event) => event.run_id.as_deref().is_none_or(accepts),
        EventKind::Provider(ProviderEvent::ProviderFallback { .. })
        | EventKind::Usage(_)
        | EventKind::Fault(_)
        | EventKind::Orchestrator(_)
        | EventKind::Ownership(_) => true,
    }
}

#[cfg(test)]
mod question_tests {
    use super::*;

    #[test]
    fn guarded_batches_acquire_once_and_only_recheck_legacy_callbacks() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        let fences = Arc::new(MutationFences::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_guard = Arc::clone(&calls);
        assert!(fences.register_nonblocking_guard(
            "guarded".into(),
            Arc::new(|| Some(Box::new(()))),
            Arc::new(move || {
                calls_for_guard.fetch_add(1, Ordering::Relaxed);
                MutationGuardAttempt::Acquired(Box::new(()))
            })
        ));
        let current = Arc::new(AtomicBool::new(true));
        let current_for_check = Arc::clone(&current);
        assert!(fences.register(
            "checked".into(),
            Arc::new(move || { current_for_check.load(Ordering::Acquire) })
        ));
        let validator = MutationValidator::new(fences);
        let events: Vec<_> = ["guarded", "guarded", "checked"]
            .map(|run| {
                Event::new(LifecycleEvent::Started {
                    session_id: run.into(),
                })
            })
            .into();
        let guards = validator.acquire_batch(&events).expect("guarded batch");
        assert_eq!(guards.accepted(), &[true, true, true]);
        current.store(false, Ordering::Release);
        let accepted = validator
            .recheck_batch(events.clone(), guards.revision())
            .unwrap();
        assert_eq!(accepted, events[..2]);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_fence_registered_after_acquisition_requires_batch_revalidation() {
        let fences = Arc::new(MutationFences::default());
        let validator = MutationValidator::new(Arc::clone(&fences));
        let events = vec![Event::new(LifecycleEvent::Started {
            session_id: "run".into(),
        })];
        let guards = validator.acquire_batch(&events).expect("initial batch");
        assert!(fences.register_guard("run".into(), Arc::new(|| None)));
        assert_eq!(
            validator.recheck_batch(events.clone(), guards.revision()),
            Err(events.clone())
        );
        let updated = validator.acquire_batch(&events).expect("revalidated batch");
        assert_eq!(updated.accepted(), &[false]);
    }

    #[test]
    fn published_prompt_and_result_preserve_the_run_fence() {
        let fences = MutationFences::default();
        assert!(fences.register("run-stale".into(), Arc::new(|| false)));
        for run in ["run-current", "run-stale"] {
            let events = [
                Event::new(LifecycleEvent::TaskPromptPublished {
                    run_id: run.into(),
                    parent_run_id: None,
                    agent_name: "Worker".into(),
                    role: "worker".into(),
                    prompt: "task".into(),
                }),
                Event::new(MessageEvent::MessageCompleted {
                    run_id: run.into(),
                    text: "answer".into(),
                }),
                Event::new(MessageEvent::FinalResultPublished {
                    run_id: run.into(),
                    text: "result".into(),
                }),
            ];
            for event in events {
                assert_eq!(fences.accepts(&event), run == "run-current");
            }
        }
    }
    #[test]
    fn question_updates_preserve_the_requester_generation_fence() {
        let fences = MutationFences::default();
        assert!(fences.register("run-1".into(), Arc::new(|| false)));
        let mut question = crate::UserQuestion {
            id: "question-1".into(),
            run_id: "run-1".into(),
            root_run_id: "run-1".into(),
            root_name: "chat:Worker:thread".into(),
            recipient_run_ids: Vec::new(),
            title: "scope".into(),
            options: vec![],
            blocking: true,
            answer: None,
        };
        assert!(!fences.accepts(&Event::new(ToolEvent::UserQuestionUpdated {
            question: question.clone()
        })));
        question.answer = Some("A".into());
        assert!(!fences.accepts(&Event::new(ToolEvent::UserQuestionUpdated { question })));
    }
}
