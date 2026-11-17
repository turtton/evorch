//! Samples of the operations the `pull_requests` area adds.
use super::AreaSamples;
use workbench_protocol::commands::Command;
use workbench_protocol::events::UiEvent;
use workbench_protocol::ids::{OperationId, ThreadId};
use workbench_protocol::namespaces::pull_requests::*;
use workbench_protocol::operation::{CancelReason, OperationState, OperationView};
use workbench_protocol::queries::{Query, QueryReply};
use workbench_protocol::wire::{Revision, Sequence, UnixMillis};

fn op(id: &str) -> OperationId {
    OperationId::new(id).unwrap()
}

fn thread() -> ThreadId {
    ThreadId::new("thread-1").unwrap()
}

fn reference() -> PullRequestRef {
    PullRequestRef {
        provider: "github".into(),
        host: "github.com".into(),
        repository: "turtton/evorch".into(),
        number: 42,
    }
}

fn operation(state: OperationState, kind: PullRequestOperationKind) -> PullRequestOperationView {
    let ready = matches!(state, OperationState::Ready);
    PullRequestOperationView {
        operation: OperationView {
            operation_id: op("op-1"),
            state,
            revision: Revision(4),
            result: None,
        },
        kind,
        thread: thread(),
        lifecycle_revision: ready.then_some(Revision(7)),
        policy_revision: None,
        notice_id: None,
        message: matches!(state, OperationState::Failed { .. })
            .then(|| "the thread's pull request changed since it was read".into()),
        outcome_unknown: false,
    }
}

pub fn thread_view() -> ThreadPullRequest {
    ThreadPullRequest {
        thread: thread(),
        tracking_enabled: true,
        owned_here: true,
        lifecycle_revision: Revision(7),
        transition_seq: Sequence(3),
        link: Some(LinkView {
            pull_request: reference(),
            link_revision: Revision(2),
            origin: LinkOrigin::Explicit,
            linked_at: UnixMillis(1_700_000_000_000),
        }),
        local_head: Some("0123456789abcdef".into()),
        snapshot: Some(SnapshotView {
            facts: Some(FactsView {
                state: PullRequestState::Open,
                base_branch: "main".into(),
                head_repository: None,
                head_branch: "feat/x".into(),
                head_sha: "0123456789abcdef".into(),
                checks: vec![CheckView {
                    name: "ci / test".into(),
                    kind: "check_run".into(),
                    status: "completed".into(),
                    conclusion: Some("failure".into()),
                }],
                checks_complete: true,
                review_decision: ReviewDecision::ReviewRequired,
            }),
            title: Some("Add the thing".into()),
            fetched_at: Some(UnixMillis(1_700_000_001_000)),
            last_failure: Some(FetchFailureView {
                kind: "rate_limited".into(),
                message: "rate limited".into(),
            }),
            consecutive_failures: 1,
        }),
        candidates: vec![CandidateView {
            candidate_id: 9,
            pull_request: reference(),
            state: CandidateState::Presented,
        }],
        auto_fix: Some(AutoFixView {
            enabled: true,
            globally_enabled: true,
            destination: Some(thread()),
            link_revision: Some(Revision(2)),
            policy_revision: Revision(5),
            used: 3,
            reserved: 0,
            limit: AUTO_FIX_LIMIT,
            cooldown_until: None,
            stop_reason: Some("limit_reached".into()),
            limit_reached: true,
        }),
        head_attribution: HeadAttribution::Agent,
    }
}

pub fn samples() -> AreaSamples {
    AreaSamples {
        name: "pull_requests",
        commands: vec![
            Command::PullRequests(PullRequestsCommand::LinkPullRequest(LinkPullRequest {
                operation_id: op("op-link"),
                thread: thread(),
                locator: "turtton/evorch#42".into(),
                expected_revision: Revision(7),
            })),
            Command::PullRequests(PullRequestsCommand::UnlinkPullRequest(UnlinkPullRequest {
                operation_id: op("op-unlink"),
                thread: thread(),
                expected_revision: Revision(8),
            })),
            Command::PullRequests(PullRequestsCommand::RefreshPullRequest(
                RefreshPullRequest {
                    operation_id: op("op-refresh"),
                    thread: thread(),
                },
            )),
            Command::PullRequests(PullRequestsCommand::ConfirmPullRequestCandidate(
                ConfirmCandidate {
                    operation_id: op("op-confirm"),
                    thread: thread(),
                    candidate_id: 9,
                    expected_revision: Revision(7),
                },
            )),
            Command::PullRequests(PullRequestsCommand::DismissPullRequestCandidate(
                DismissCandidate {
                    operation_id: op("op-dismiss"),
                    thread: thread(),
                    candidate_id: 9,
                },
            )),
            Command::PullRequests(PullRequestsCommand::SetAutoFix(SetAutoFix {
                operation_id: op("op-auto-fix"),
                thread: thread(),
                link_revision: Revision(2),
                policy_revision: Revision(5),
                enabled: true,
            })),
            Command::PullRequests(PullRequestsCommand::ResetAutoFixBudget(
                ResetAutoFixBudget {
                    operation_id: op("op-reset"),
                    thread: thread(),
                    link_revision: Revision(2),
                    policy_revision: Revision(5),
                    confirmed_previous: Some(ThreadId::new("thread-0").unwrap()),
                },
            )),
            Command::PullRequests(PullRequestsCommand::ChangeAutoFixDestination(
                ChangeAutoFixDestination {
                    operation_id: op("op-destination"),
                    thread: thread(),
                    link_revision: Revision(2),
                    policy_revision: Revision(5),
                    confirmed_previous: None,
                },
            )),
            Command::PullRequests(PullRequestsCommand::RequestManualNotice(
                RequestManualNotice {
                    operation_id: op("op-notice"),
                    thread: thread(),
                    link_revision: Revision(2),
                    expected_head: "0123456789abcdef".into(),
                },
            )),
        ],
        queries: vec![
            Query::PullRequests(PullRequestsQuery::GetThreadPullRequest { thread: thread() }),
            Query::PullRequests(PullRequestsQuery::GetPullRequestNotice {
                notice_id: "notice-1".into(),
            }),
            Query::PullRequests(PullRequestsQuery::GetPullRequestOperation {
                operation_id: op("op-1"),
            }),
            Query::PullRequests(PullRequestsQuery::ListPullRequestOperations {}),
        ],
        ui_events: vec![
            UiEvent::PullRequests(PullRequestsEvent::PullRequestChanged {
                thread: thread(),
                pull_request: reference(),
                change: PullRequestChange::FactsChanged,
                link_revision: Revision(2),
                transition_seq: Sequence(3),
                notifications: vec!["checks_failed".into()],
            }),
            UiEvent::PullRequests(PullRequestsEvent::AutoFixLimitReached {
                thread: thread(),
                pull_request: reference(),
                link_revision: Revision(2),
                budget_epoch: 1,
                limit: AUTO_FIX_LIMIT,
            }),
            UiEvent::PullRequests(PullRequestsEvent::PullRequestOperationCompleted {
                operation: operation(OperationState::Ready, PullRequestOperationKind::Link),
            }),
        ],
        query_replies: vec![
            QueryReply::PullRequests(PullRequestsReply::Thread(Box::new(thread_view()))),
            QueryReply::PullRequests(PullRequestsReply::Notice(Box::new(NoticeView {
                notice_id: "notice-1".into(),
                pull_request: reference(),
                head_sha: "0123456789abcdef".into(),
                mode: NoticeMode::Auto,
                destination: Some(thread()),
                link_revision: Revision(2),
                policy_revision: Revision(5),
                phase: NoticePhase::Ready,
                payload: Some("checks failed".into()),
            }))),
            QueryReply::PullRequests(PullRequestsReply::Operation(operation(
                OperationState::Pending,
                PullRequestOperationKind::Refresh,
            ))),
            QueryReply::PullRequests(PullRequestsReply::Operations {
                operations: vec![
                    operation(
                        OperationState::Failed {
                            code: workbench_protocol::ErrorCode::StaleRevision,
                        },
                        PullRequestOperationKind::Unlink,
                    ),
                    operation(
                        OperationState::Cancelled {
                            reason: CancelReason::Shutdown,
                        },
                        PullRequestOperationKind::Link,
                    ),
                ],
            }),
        ],
    }
}
