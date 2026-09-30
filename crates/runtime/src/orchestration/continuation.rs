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
        // A stop preserves the cursor but only explicit history restore may resume it.
        TaskStatus::Stopped => Some(SuppressReason::Paused),
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

#[cfg(test)]
mod tests {
    use super::super::ledger::GoalLedger;
    use super::*;
    use event_bus::OrchestratorEvent;
    use storage::entity::{TaskContinuation, TaskStatus};

    fn snapshot() -> GoalSnapshot {
        GoalLedger::new(&OrchestratorEvent::GoalCreated {
            goal_id: "goal".into(),
            session_id: "session".into(),
            project_id: "project".into(),
            thread_id: "thread".into(),
            goal: "work".into(),
            references: vec![],
            constraints: vec![],
            repo: "repo".into(),
            base_ref: "main".into(),
            root_run_id: "run-1".into(),
        })
        .snapshot()
        .clone()
    }

    #[test]
    fn stopped_task_suppresses_continuation_without_closing_cursor() {
        let task = TaskContinuation {
            status: TaskStatus::Stopped,
            input: Some("work".into()),
            resume_cursor: Some("saved cursor".into()),
            last_artifact: Some("partial artifact".into()),
            failure_reason: Some("stopped".into()),
            attempts: 0,
            heartbeat_at_ns: Some(1),
        };
        for max_attempts in [0, 10] {
            assert_eq!(
                decide_task(&task, max_attempts),
                ContinuationDecision::Suppress(SuppressReason::Paused)
            );
        }
        assert_eq!(task.resume_cursor.as_deref(), Some("saved cursor"));
    }

    #[test]
    fn paused_goal_cannot_dispatch_even_with_terminal_orchestrator() {
        let mut snapshot = snapshot();
        snapshot.state = GoalState::Paused;
        for pipeline_busy in [false, true] {
            assert_eq!(
                decide(
                    &snapshot,
                    true,
                    pipeline_busy,
                    &OrchestrationSettings::default()
                ),
                Some(ContinuationDecision::Suppress(SuppressReason::Paused))
            );
        }
    }

    #[test]
    fn live_resumed_orchestrator_prevents_dispatch_at_any_epoch() {
        let mut snapshot = snapshot();
        snapshot.epoch = 3;
        assert_eq!(
            decide(&snapshot, false, false, &OrchestrationSettings::default()),
            None
        );
        // The caller must report live registration as nonterminal: epoch alone
        // is not a liveness predicate and would allow a duplicate generation.
        assert_eq!(
            decide(&snapshot, true, false, &OrchestrationSettings::default()),
            Some(ContinuationDecision::Dispatch)
        );
    }
}
