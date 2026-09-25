use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use crate::{
    AgentMessageEvent, CompactionEvent, Event, EventKind, LifecycleEvent, MessageEvent,
    ProviderEvent, ToolEvent,
};

pub type MutationCheck = Arc<dyn Fn() -> bool + Send + Sync>;
pub trait MutationGuard: Send {}
impl<T: Send> MutationGuard for T {}
pub type MutationGuardCheck = Arc<dyn Fn() -> Option<Box<dyn MutationGuard>> + Send + Sync>;

enum Fence {
    Check(MutationCheck),
    Guard(MutationGuardCheck),
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

    pub(crate) fn new(fences: Arc<MutationFences>) -> Self {
        Self(fences)
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
        let guards = std::cell::RefCell::new(Vec::new());
        if runs.is_empty() {
            return Some(guards.into_inner());
        }
        let accepts = |run: &str| match runs.get(run) {
            None => true,
            Some(Fence::Check(check)) => check(),
            Some(Fence::Guard(check)) => match check() {
                Some(guard) => {
                    guards.borrow_mut().push(guard);
                    true
                }
                None => false,
            },
        };
        let accepted = match &event.kind {
            EventKind::Lifecycle(event) => match event {
                LifecycleEvent::RunProgress { run_id, .. }
                | LifecycleEvent::AgentRunStarted { run_id, .. }
                | LifecycleEvent::TaskPromptPublished { run_id, .. }
                | LifecycleEvent::AgentRunRestored { run_id, .. }
                | LifecycleEvent::AgentRunStateChanged { run_id, .. }
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
                MessageEvent::MessageDelta { run_id, .. }
                | MessageEvent::ReasoningDelta { run_id, .. },
            )
            | EventKind::Tool(
                ToolEvent::ToolStarted { run_id, .. } | ToolEvent::ToolCompleted { run_id, .. },
            )
            | EventKind::Provider(
                ProviderEvent::RequestStarted { run_id, .. }
                | ProviderEvent::FirstTokenObserved { run_id, .. }
                | ProviderEvent::RequestCompleted { run_id, .. }
                | ProviderEvent::RequestFailed { run_id, .. },
            ) => run_id.as_deref().is_none_or(accepts),
            EventKind::Message(
                MessageEvent::FinalResultPublished { run_id, .. }
                | MessageEvent::MessageCompleted { run_id, .. },
            ) => accepts(run_id),
            EventKind::Tool(ToolEvent::UserQuestionUpdated { question }) => {
                accepts(&question.run_id)
            }
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
            EventKind::Ledger(crate::LedgerEvent::RunLedgerAppended { run_id, .. }) => {
                accepts(run_id)
            }
            EventKind::Diagnostic(event) => event.run_id.as_deref().is_none_or(accepts),
            EventKind::Provider(ProviderEvent::ProviderFallback { .. })
            | EventKind::Usage(_)
            | EventKind::Fault(_)
            | EventKind::Orchestrator(_)
            | EventKind::Ownership(_) => true,
        };
        accepted.then(|| guards.into_inner())
    }
}

#[cfg(test)]
mod question_tests {
    use super::*;

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
