use event_bus::{CloseoutStep, GoalStage, GoalState};

pub(crate) const fn state_label(state: GoalState) -> &'static str {
    match state {
        GoalState::Active => "active",
        GoalState::Paused => "paused",
        GoalState::Blocked => "blocked",
        GoalState::Complete => "complete",
        GoalState::Cancelled => "cancelled",
    }
}

pub(crate) const fn stage_label(stage: GoalStage) -> &'static str {
    match stage {
        GoalStage::Implementing => "implementing",
        GoalStage::Delivering => "delivering",
        GoalStage::AwaitingCi => "awaiting_ci",
        GoalStage::Reviewing => "reviewing",
        GoalStage::Repairing => "repairing",
        GoalStage::ReadyToFinish => "ready_to_finish",
        GoalStage::AwaitingMergeApproval => "awaiting_merge_approval",
        GoalStage::Merging => "merging",
        GoalStage::Closeout => "closeout",
        GoalStage::Done => "done",
    }
}

pub(crate) const fn step_label(step: CloseoutStep) -> &'static str {
    match step {
        CloseoutStep::WorkerClaim => "worker_claim",
        CloseoutStep::ResultSummary => "result_summary",
        CloseoutStep::WorkerComplete => "worker_complete",
    }
}
