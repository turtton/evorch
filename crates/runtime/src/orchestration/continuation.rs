//! idle epoch ごとの continuation 判定。

use event_bus::{GoalState, SuppressReason};

use super::ledger::{GoalSnapshot, OrchestrationSettings};

/// continuation 判定結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContinuationDecision {
    /// 新しい orchestrator run を起動する。
    Dispatch,
    /// 条件により起動しない。
    Suppress(SuppressReason),
}

pub fn decide_task(
    task: &storage::entity::TaskContinuation,
    max_attempts: u32,
) -> ContinuationDecision {
    use storage::entity::TaskStatus;
    let reason = match task.status {
        TaskStatus::Cancelled => Some(SuppressReason::Cancelled),
        TaskStatus::Completed => Some(SuppressReason::Complete),
        TaskStatus::Running | TaskStatus::Retrying => Some(SuppressReason::Duplicate),
        TaskStatus::Pending | TaskStatus::Queued | TaskStatus::Blocked | TaskStatus::Failed => None,
    };
    if let Some(reason) = reason {
        return ContinuationDecision::Suppress(reason);
    }
    if task
        .failure_reason
        .as_deref()
        .is_some_and(is_configuration_failure)
    {
        return ContinuationDecision::Suppress(SuppressReason::Blocked);
    }
    if task.attempts >= max_attempts {
        return ContinuationDecision::Suppress(SuppressReason::LimitReached { max: max_attempts });
    }
    ContinuationDecision::Dispatch
}

pub(crate) fn is_configuration_failure(reason: &str) -> bool {
    reason == crate::RuntimeError::WorkspaceContextRequired.to_string()
}

/// 現在の epoch を dispatch できるかを副作用なしで判定する。
pub fn decide(
    snapshot: &GoalSnapshot,
    orchestrator_terminal: bool,
    pipeline_busy: bool,
    settings: &OrchestrationSettings,
) -> Option<ContinuationDecision> {
    if !orchestrator_terminal {
        return None;
    }
    let state_reason = match snapshot.state {
        GoalState::Active => None,
        GoalState::Paused => Some(SuppressReason::Paused),
        GoalState::Blocked => Some(SuppressReason::Blocked),
        GoalState::Complete => Some(SuppressReason::Complete),
        GoalState::Cancelled => Some(SuppressReason::Cancelled),
    };
    if let Some(reason) = state_reason {
        return Some(ContinuationDecision::Suppress(reason));
    }
    if snapshot.dispatched_epochs.contains(&snapshot.epoch) {
        return Some(ContinuationDecision::Suppress(SuppressReason::Duplicate));
    }
    if snapshot.dispatched_epochs.len() >= settings.max_continuations as usize {
        return Some(ContinuationDecision::Suppress(
            SuppressReason::LimitReached {
                max: settings.max_continuations,
            },
        ));
    }
    if pipeline_busy {
        return Some(ContinuationDecision::Suppress(SuppressReason::PipelineBusy));
    }
    Some(ContinuationDecision::Dispatch)
}
