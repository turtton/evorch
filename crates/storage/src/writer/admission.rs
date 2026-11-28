//! G1–G7 storage mutation routing. This match is exhaustive: a new writer
//! command cannot silently bypass admission. The callback is memory-only.
use super::Command;
use event_bus::{Event, EventKind, OrchestratorEvent};

pub(super) fn scope(command: &Command) -> Option<Option<String>> {
    match command {
        Command::AppendHostInputAccepted(_, _, _, receipt, _) => {
            Some(Some(format!("r:{}", receipt.run_id)))
        }
        Command::RecordHostChatSubmission(submission, _) => {
            Some(Some(format!("r:{}", submission.run_id)))
        }
        // G3/G4: the destination of an uncommitted binding is not yet
        // discoverable by a family reader. Register globally, without
        // expanding that family or inspecting serialized context.
        Command::UpsertRunContext(..)
        | Command::UpsertRunContextWithEvidence(..)
        | Command::UpsertRunContextWithReceipts(..)
        | Command::AdvanceHandoff(..)
        | Command::ConversationPlan(..)
        | Command::SaveResumeIntent(..)
        | Command::RetireResumeIntent(..)
        | Command::StopUnregisteredResumeIntent(..)
        | Command::Recovery(..)
        | Command::CompareAndSwapRunContextRestoreState(..)
        | Command::InvalidateRunContext(..)
        | Command::RecordReviewEvidenceWithEdges(..)
        | Command::RecordReviewLineageEdge(..)
        | Command::TransitionHostInputs(..)
        | Command::UpdateHostInputReceipts(..)
        | Command::CreateUserQuestion(..)
        | Command::BindUserQuestions(..)
        | Command::AnswerUserQuestion(..)
        | Command::AnswerUserQuestionWithEvidence(..)
        | Command::SaveEvalSample(..)
        | Command::DeleteEvalSample(..)
        | Command::UpsertThreads(..)
        | Command::Reconcile(..) => Some(None),
        Command::AppendEvent(_, event, _) | Command::AppendStreamEvent(_, event, ..) => {
            event_scope(event)
        }
        // Notes, usage, index/diagnostic/reclaim maintenance and reads do not
        // admit execution, change membership or alter canonical protections.
        // Snapshot and lane commands have their own ACK-scoped wrappers.
        Command::History(..)
        | Command::UpsertThreadGoalUsage(..)
        | Command::Usage(..)
        | Command::RecordUsageRequests(..)
        | Command::AttributeUsageRuns(..)
        | Command::RecordCatalogUpdate(..)
        | Command::Memory(..)
        | Command::RecordImprovementCandidate(..)
        | Command::SetImprovementStatus(..)
        | Command::AttachImprovementDraft(..)
        | Command::UpdateMemory(..)
        | Command::UpdateImprovement(..)
        | Command::TaskQueue(..)
        | Command::AppendRunLedger(..)
        | Command::LatestRunLedger(..)
        | Command::SaveAssistantReference(..)
        | Command::AssistantReference(..)
        | Command::ClaimLearning(..)
        | Command::LearningJob(..)
        | Command::SaveThreadStartState(..)
        | Command::FlushResumeIntents(..)
        | Command::AppendIndexedNote(..)
        | Command::SaveSnapshotBundle(..)
        | Command::SaveSnapshotBundleWithResumeIntent(..)
        | Command::SaveSnapshotBundleWithEvidence(..)
        | Command::HostInputReceipts(..)
        | Command::HostChatSubmissions(..)
        | Command::TombstonedRuns(..)
        | Command::HostInputReceiptsForRun(..)
        | Command::FlushUsage(..)
        | Command::Checkpoint(..)
        | Command::Index(..)
        | Command::Statistics(..)
        | Command::ReclaimStep(..)
        | Command::ReclaimMarkFiles(..)
        | Command::RecordArtifactRef(..)
        | Command::RemoveArtifactRefs(..)
        | Command::RetainToolBodies(..)
        | Command::ConfirmRetentionOnline(..)
        | Command::EnsureRetentionBinding(..)
        | Command::RetentionBackfillStep(..)
        | Command::DiagnosticCleanupStatus(..)
        | Command::CleanupDiagnosticBatch(..)
        | Command::ReclaimDiagnosticSpace(..)
        | Command::Lane(..)
        | Command::PullRequest(..)
        | Command::AdminAudit(..)
        | Command::Shutdown => None,
    }
}

fn event_scope(event: &Event) -> Option<Option<String>> {
    if let EventKind::Orchestrator(OrchestratorEvent::ThreadGoalUpdated { snapshot }) = &event.kind
    {
        return Some(Some(format!("t:{}", snapshot.thread_id)));
    }
    let relevant = matches!(
        (
            crate::repo::event::kind_name(&event.kind),
            event.kind.subkind_name()
        ),
        (
            "Message",
            "MessageCompleted" | "MessagePart" | "FinalResultPublished"
        ) | ("Lifecycle", "AgentRunStateChanged" | "TurnCompleted")
            | ("AgentMessage", "Delivered")
    );
    relevant.then(|| {
        event
            .primary_run_id()
            .filter(|run| !run.is_empty())
            .map(|run| format!("r:{run}"))
    })
}

#[cfg(test)]
mod entries;
