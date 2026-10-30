//! Which runs an event must be stored after.
//!
//! Reasoning rows are written ahead of every later event of the same run, so
//! replay sees them where they happened. This is deliberately separate from the
//! mutation fences' attribution, which keeps its own authority semantics. The
//! match names every variant of every event family, so adding one forces a
//! decision here instead of silently reordering rows.

use crate::{
    AgentMessageEvent, CompactionEvent, Event, EventKind, FaultEvent, LedgerEvent, LifecycleEvent,
    MessageEvent, OrchestratorEvent, ProviderEvent, ToolEvent, UsageEvent,
};

/// Run, session and task identifiers `event` belongs to; empty when it belongs
/// to none. Session and task ids are included because the mutation fences key
/// on them and an extra identifier only costs an early flush.
pub fn event_runs(event: &Event) -> Vec<&str> {
    let mut runs: Vec<&str> = Vec::new();
    match &event.kind {
        EventKind::Lifecycle(event) => match event {
            LifecycleEvent::RunProgress { run_id, .. }
            | LifecycleEvent::AgentRunStarted { run_id, .. }
            | LifecycleEvent::TaskPromptPublished { run_id, .. }
            | LifecycleEvent::AgentRunStateChanged { run_id, .. }
            | LifecycleEvent::TurnCompleted { run_id, .. }
            | LifecycleEvent::EscalationProposed { run_id, .. }
            | LifecycleEvent::AgentRunRestored { run_id, .. }
            | LifecycleEvent::RootWakeRequested { run_id, .. }
            | LifecycleEvent::AutoContinuationPaused { run_id, .. }
            | LifecycleEvent::HostInputDelivered { run_id, .. }
            | LifecycleEvent::HostInputDropped { run_id, .. }
            | LifecycleEvent::HostInputUnknown { run_id, .. } => runs.push(run_id),
            LifecycleEvent::BackgroundTaskStarted { task_id }
            | LifecycleEvent::BackgroundTaskCompleted { task_id }
            | LifecycleEvent::BackgroundTaskCancelled { task_id } => runs.push(task_id),
            LifecycleEvent::Started { session_id, .. }
            | LifecycleEvent::Delegated { session_id, .. }
            | LifecycleEvent::Completed { session_id, .. }
            | LifecycleEvent::Failed { session_id, .. } => runs.push(session_id),
            LifecycleEvent::EscalationRequested {
                source_run_id,
                new_run_id,
                ..
            } => runs.extend([source_run_id.as_str(), new_run_id.as_str()]),
            LifecycleEvent::RoutingDecision { .. } => {}
        },
        EventKind::Message(event) => match event {
            MessageEvent::MessageDelta { run_id, .. }
            | MessageEvent::ReasoningDelta { run_id, .. }
            | MessageEvent::ToolCallDelta { run_id, .. }
            | MessageEvent::StreamRetried { run_id, .. } => runs.extend(run_id.as_deref()),
            MessageEvent::MessageCompleted { run_id, .. }
            | MessageEvent::MessagePart { run_id, .. }
            | MessageEvent::FinalResultPublished { run_id, .. } => runs.push(run_id),
        },
        EventKind::Tool(event) => match event {
            ToolEvent::ToolStarted { run_id, .. }
            | ToolEvent::ToolCompleted { run_id, .. }
            | ToolEvent::ShellJobOutput { run_id, .. } => runs.extend(run_id.as_deref()),
            ToolEvent::ArtifactsPresented { run_id, .. } => runs.push(run_id),
            ToolEvent::UserQuestionUpdated { question } => {
                runs.extend([question.run_id.as_str(), question.root_run_id.as_str()]);
            }
            // Call ids are "<run id>:<call>".
            ToolEvent::ApprovalRequested { call_id, .. }
            | ToolEvent::ApprovalResolved { call_id, .. }
            | ToolEvent::ExecutionDenied { call_id, .. } => {
                runs.extend(call_id.split_once(':').map(|(run, _)| run));
            }
        },
        EventKind::Provider(event) => match event {
            ProviderEvent::RequestStarted { run_id, .. }
            | ProviderEvent::FirstTokenObserved { run_id, .. }
            | ProviderEvent::RequestCompleted { run_id, .. }
            | ProviderEvent::CacheReuseObserved { run_id, .. }
            | ProviderEvent::RequestFailed { run_id, .. }
            | ProviderEvent::CooldownStarted { run_id, .. } => runs.extend(run_id.as_deref()),
            ProviderEvent::FallbackTriggered { session_id, .. } => runs.push(session_id),
            ProviderEvent::ProviderFallback { .. } => {}
        },
        EventKind::AgentMessage(event) => match event {
            AgentMessageEvent::Delivered { message, .. } => runs.extend([
                message.sender_run_id.as_str(),
                message.recipient_run_id.as_str(),
            ]),
        },
        EventKind::Compaction(event) => match event {
            CompactionEvent::Compacted { run_id, .. } => runs.push(run_id),
        },
        EventKind::Ledger(event) => match event {
            LedgerEvent::RunLedgerAppended { run_id, .. } => runs.push(run_id),
        },
        EventKind::Diagnostic(event) => runs.extend(event.run_id.as_deref()),
        EventKind::Orchestrator(event) => match event {
            OrchestratorEvent::TaskProgressed { run_id, .. }
            | OrchestratorEvent::TaskCheckpoint { run_id, .. } => runs.push(run_id),
            OrchestratorEvent::ThreadGoalUpdated { .. }
            | OrchestratorEvent::ThreadTodoUpdated { .. } => {}
        },
        EventKind::Usage(event) => match event {
            UsageEvent::Usage { .. } | UsageEvent::CacheStats { .. } => {}
        },
        EventKind::Fault(event) => match event {
            FaultEvent::SubscriberLagged { .. } | FaultEvent::SkillDiagnostic { .. } => {}
        },
        EventKind::Ownership(_) => {}
    }
    runs
}
