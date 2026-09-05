use event_bus::{
    CiState, CloseoutStep, GateRejection, GateSnapshot, GoalStage, GoalState, MergeBinding,
    OrchestratorEvent,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReferenceKind {
    Packet,
    Issue,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PacketReference {
    pub kind: ReferenceKind,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalSubmission {
    pub project_id: String,
    pub thread_id: String,
    pub goal: String,
    pub references: Vec<PacketReference>,
    pub constraints: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MergeDecision {
    Approve,
    Reject { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrRef {
    pub number: u64,
    pub title: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeCommand {
    pub thread_id: String,
    pub pr: Option<PrRef>,
    pub token_id: Option<String>,
    pub decision: MergeDecision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkbenchCommand {
    SubmitGoal(GoalSubmission),
    DecideMerge(MergeCommand),
    PauseGoal { goal_id: String },
    ResumeGoal { goal_id: String },
    CancelGoal { goal_id: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CiStatus {
    Unknown,
    Pending,
    Passing,
    Failing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewerStatus {
    Unknown,
    Pending,
    Approved,
    ChangesRequested,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeApprovalView {
    pub pr: Option<PrRef>,
    pub ci: CiStatus,
    pub reviewer: ReviewerStatus,
    pub diff_summary: Option<String>,
    pub resolution: Option<MergeDecision>,
    /// 承認トークン束縛 (`MergeApprovalRequested` で確定し、Approve の必須条件)。
    pub binding: Option<MergeBinding>,
    /// gate チェックリストの表示行。
    pub gate: Vec<GateItemView>,
    /// goal が Blocked になるなど Approve を不能にする事由。
    pub blocked: Option<String>,
}

/// gate チェックリスト 1 行の表示モデル。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateItemView {
    pub label: String,
    pub ok: bool,
    pub detail: String,
}

/// goal ループ状態の表示モデル (既定は空)。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopStatusView {
    pub goal_id: Option<String>,
    pub state: Option<GoalState>,
    pub stage: Option<GoalStage>,
    pub review_round: u32,
    pub nudges: u32,
    pub epoch: u64,
    pub last_rejections: Vec<String>,
    pub closeout: Vec<(CloseoutStep, bool)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LoopEvent {
    GoalAccepted {
        thread_id: String,
        goal_id: String,
    },
    MergeStateUpdated(MergeApprovalView),
    MergeResolved {
        thread_id: String,
        decision: MergeDecision,
    },
    LoopStatusUpdated(LoopStatusView),
    CommandRejected {
        reason: String,
    },
}

pub trait CommandSink: Send {
    fn submit(&mut self, cmd: WorkbenchCommand) -> Vec<LoopEvent>;
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecordingSink {
    pub issued: Vec<WorkbenchCommand>,
}

impl CommandSink for RecordingSink {
    fn submit(&mut self, cmd: WorkbenchCommand) -> Vec<LoopEvent> {
        self.issued.push(cmd);
        Vec::new()
    }
}

#[derive(Debug, Clone, Default)]
pub struct FixtureLoopAdapter {
    accepted_goals: u64,
}

impl FixtureLoopAdapter {
    fn fixture_view(resolution: Option<MergeDecision>) -> MergeApprovalView {
        MergeApprovalView {
            pr: Some(PrRef {
                number: 65,
                title: "Workbench restructure".into(),
                url: "https://github.com/turtton/evorch/pull/65".into(),
            }),
            ci: CiStatus::Pending,
            reviewer: ReviewerStatus::Pending,
            diff_summary: Some("model-only change".into()),
            resolution,
            binding: None,
            gate: Vec::new(),
            blocked: None,
        }
    }
}

impl CommandSink for FixtureLoopAdapter {
    fn submit(&mut self, cmd: WorkbenchCommand) -> Vec<LoopEvent> {
        match cmd {
            WorkbenchCommand::SubmitGoal(submission) => {
                self.accepted_goals = self.accepted_goals.saturating_add(1);
                vec![
                    LoopEvent::GoalAccepted {
                        thread_id: submission.thread_id,
                        goal_id: format!("goal-{}", self.accepted_goals),
                    },
                    LoopEvent::MergeStateUpdated(Self::fixture_view(None)),
                ]
            }
            WorkbenchCommand::DecideMerge(command) => vec![LoopEvent::MergeResolved {
                thread_id: command.thread_id,
                decision: command.decision,
            }],
            // 一時停止/再開/取消の結果はバス上の OrchestratorEvent として届くため、
            // fixture は即応イベントを発行しない。
            WorkbenchCommand::PauseGoal { .. }
            | WorkbenchCommand::ResumeGoal { .. }
            | WorkbenchCommand::CancelGoal { .. } => Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GoalFormModel {
    pub goal: String,
    pub references: Vec<PacketReference>,
    pub constraints: Vec<String>,
    pub last_accepted: Option<String>,
}

impl GoalFormModel {
    pub fn build_command(&self, project_id: &str, thread_id: &str) -> WorkbenchCommand {
        WorkbenchCommand::SubmitGoal(GoalSubmission {
            project_id: project_id.into(),
            thread_id: thread_id.into(),
            goal: self.goal.clone(),
            references: self.references.clone(),
            constraints: self.constraints.clone(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeApprovalModel {
    pub view: MergeApprovalView,
}

impl MergeApprovalModel {
    pub fn decide(&mut self, decision: MergeDecision) -> Option<WorkbenchCommand> {
        let token_id = self.view.binding.as_ref()?.token_id.clone();
        if self.view.resolution.is_some()
            || matches!(&decision, MergeDecision::Reject { reason } if reason.trim().is_empty())
        {
            return None;
        }
        self.view.resolution = Some(decision.clone());
        Some(WorkbenchCommand::DecideMerge(MergeCommand {
            thread_id: String::new(),
            pr: self.view.pr.clone(),
            token_id: Some(token_id),
            decision,
        }))
    }
}

/// `OrchestratorEvent` を merge 承認ビューと goal ループ状態ビューへ折り畳む純粋な reducer。
///
/// GUI の `drain_pump` とヘッドレステストの両方が使用する単一の折り畳み規約。
/// `MergeApprovalRequested` で binding と gate チェックリスト行を確定させ、
/// `FinishRejected` (rounds exhausted) / `GoalStateChanged { Blocked }` で Approve を
/// 不能にする `blocked` 理由を設定する。
pub fn apply_orchestrator_event(
    view: &mut MergeApprovalView,
    status: &mut LoopStatusView,
    ev: &OrchestratorEvent,
) {
    if let Some(goal_id) = orchestrator_goal_id(ev) {
        status.goal_id = Some(goal_id.to_owned());
    }
    match ev {
        OrchestratorEvent::GoalCreated { .. } => {
            *status = LoopStatusView {
                goal_id: status.goal_id.clone(),
                state: Some(GoalState::Active),
                ..LoopStatusView::default()
            };
        }
        OrchestratorEvent::GoalStateChanged { to, reason, .. } => {
            status.state = Some(*to);
            if *to == GoalState::Blocked {
                view.blocked = Some(reason.clone());
            } else {
                view.blocked = None;
            }
        }
        OrchestratorEvent::GoalStageChanged { to, .. } => status.stage = Some(*to),
        OrchestratorEvent::ReviewRoundStarted { round, .. } => status.review_round = *round,
        OrchestratorEvent::NudgeSent { nudge_index, .. } => status.nudges = *nudge_index,
        OrchestratorEvent::ContinuationDispatched { epoch, .. } => status.epoch = *epoch,
        OrchestratorEvent::FinishRejected { rejections, .. } => {
            status.last_rejections = rejections.iter().map(rejection_label).collect();
            if rejections
                .iter()
                .any(|rejection| matches!(rejection, GateRejection::ReviewRoundsExhausted { .. }))
            {
                view.blocked = Some("review_rounds_exhausted".into());
            }
        }
        OrchestratorEvent::MergeApprovalRequested { binding, .. } => {
            view.ci = ci_status_of(&binding.snapshot.ci);
            view.binding = Some(binding.clone());
            view.gate = gate_rows(&binding.snapshot);
            view.blocked = None;
        }
        OrchestratorEvent::MergeApprovalInvalidated { reason, .. } => {
            view.binding = None;
            view.resolution = None;
            view.blocked = Some(invalidation_label(reason).to_owned());
        }
        OrchestratorEvent::CloseoutStepRecorded { step, ok, .. } => {
            if let Some(entry) = status
                .closeout
                .iter_mut()
                .find(|(recorded, _)| recorded == step)
            {
                entry.1 = *ok;
            } else {
                status.closeout.push((*step, *ok));
            }
        }
        _ => {}
    }
}

fn orchestrator_goal_id(ev: &OrchestratorEvent) -> Option<&str> {
    match ev {
        OrchestratorEvent::GoalCreated { goal_id, .. }
        | OrchestratorEvent::GoalStateChanged { goal_id, .. }
        | OrchestratorEvent::GoalStageChanged { goal_id, .. }
        | OrchestratorEvent::RunAttached { goal_id, .. }
        | OrchestratorEvent::DeliverableBranchBound { goal_id, .. }
        | OrchestratorEvent::EvidenceRecorded { goal_id, .. }
        | OrchestratorEvent::FinishRejected { goal_id, .. }
        | OrchestratorEvent::FinishAccepted { goal_id, .. }
        | OrchestratorEvent::ContinuationDispatched { goal_id, .. }
        | OrchestratorEvent::ContinuationSuppressed { goal_id, .. }
        | OrchestratorEvent::ReviewRoundStarted { goal_id, .. }
        | OrchestratorEvent::RepairDispatched { goal_id, .. }
        | OrchestratorEvent::StallDetected { goal_id, .. }
        | OrchestratorEvent::NudgeSent { goal_id, .. }
        | OrchestratorEvent::MergeApprovalRequested { goal_id, .. }
        | OrchestratorEvent::MergeApprovalResolved { goal_id, .. }
        | OrchestratorEvent::MergeApprovalInvalidated { goal_id, .. }
        | OrchestratorEvent::MergeExecuted { goal_id, .. }
        | OrchestratorEvent::CloseoutStepRecorded { goal_id, .. } => Some(goal_id),
        OrchestratorEvent::ShellCommandDenied { .. } => None,
    }
}

fn rejection_label(rejection: &GateRejection) -> String {
    match rejection {
        GateRejection::NoGoalBound => "no_goal_bound",
        GateRejection::NoDeliverableBranch => "no_deliverable_branch",
        GateRejection::NoPullRequest => "no_pull_request",
        GateRejection::PullRequestRepoMismatch { .. } => "pull_request_repo_mismatch",
        GateRejection::PullRequestBaseMismatch { .. } => "pull_request_base_mismatch",
        GateRejection::StaleHead { .. } => "stale_head",
        GateRejection::CiMissing { .. } => "ci_missing",
        GateRejection::CiPending { .. } => "ci_pending",
        GateRejection::CiFailing { .. } => "ci_failing",
        GateRejection::CriteriaUnverified { .. } => "criteria_unverified",
        GateRejection::CriteriaUnmet { .. } => "criteria_unmet",
        GateRejection::ReviewMissing { .. } => "review_missing",
        GateRejection::ReviewRequestUpdate { .. } => "review_request_update",
        GateRejection::ReviewStale { .. } => "review_stale",
        GateRejection::ReviewRoundsExhausted { .. } => "review_rounds_exhausted",
    }
    .to_owned()
}

fn invalidation_label(reason: &event_bus::InvalidationReason) -> &'static str {
    match reason {
        event_bus::InvalidationReason::HeadChanged { .. } => "stale_head",
        event_bus::InvalidationReason::CiChanged => "stale_ci",
        event_bus::InvalidationReason::ReviewChanged => "stale_review",
        event_bus::InvalidationReason::Consumed => "approval_consumed",
        event_bus::InvalidationReason::Rejected => "approval_rejected",
        event_bus::InvalidationReason::GoalNotActive => "goal_not_active",
    }
}

fn ci_status_of(state: &CiState) -> CiStatus {
    match state {
        CiState::Pending => CiStatus::Pending,
        CiState::Green => CiStatus::Passing,
        CiState::Failing { .. } => CiStatus::Failing,
    }
}

fn ci_detail(state: &CiState) -> String {
    match state {
        CiState::Pending => "pending".into(),
        CiState::Green => "green".into(),
        CiState::Failing { summary } => format!("failing: {summary}"),
    }
}

fn gate_rows(snapshot: &GateSnapshot) -> Vec<GateItemView> {
    vec![
        GateItemView {
            label: "pull_request".into(),
            ok: true,
            detail: format!("#{} ({})", snapshot.pr_number, snapshot.repo),
        },
        GateItemView {
            label: "ci".into(),
            ok: matches!(snapshot.ci, CiState::Green),
            detail: ci_detail(&snapshot.ci),
        },
        GateItemView {
            label: "criteria".into(),
            ok: true,
            detail: format!("criteria round {}", snapshot.criteria_round),
        },
        GateItemView {
            label: "review".into(),
            ok: true,
            detail: format!(
                "review round {} by {}",
                snapshot.review_round, snapshot.reviewer_run_id
            ),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending_view() -> MergeApprovalView {
        MergeApprovalView {
            pr: Some(PrRef {
                number: 65,
                title: "Workbench restructure".into(),
                url: "https://github.com/turtton/evorch/pull/65".into(),
            }),
            ci: CiStatus::Pending,
            reviewer: ReviewerStatus::Pending,
            diff_summary: Some("model-only change".into()),
            resolution: None,
            binding: None,
            gate: Vec::new(),
            blocked: None,
        }
    }

    #[test]
    fn goal_submission_serializes_references_and_constraints() {
        let command = WorkbenchCommand::SubmitGoal(GoalSubmission {
            project_id: "evorch".into(),
            thread_id: "thread-1".into(),
            goal: "implement issue".into(),
            references: vec![PacketReference {
                kind: ReferenceKind::Issue,
                value: "65".into(),
            }],
            constraints: vec!["model only".into()],
        });

        let json = serde_json::to_string(&command).expect("serialize command");
        let decoded: WorkbenchCommand = serde_json::from_str(&json).expect("deserialize command");

        assert_eq!(decoded, command);
    }

    #[test]
    fn fixture_adapter_accepts_goal_and_publishes_pending_merge_view() {
        let mut adapter = FixtureLoopAdapter::default();
        let events = adapter.submit(WorkbenchCommand::SubmitGoal(GoalSubmission {
            project_id: "evorch".into(),
            thread_id: "thread-1".into(),
            goal: "implement issue".into(),
            references: Vec::new(),
            constraints: Vec::new(),
        }));

        assert_eq!(
            events,
            vec![
                LoopEvent::GoalAccepted {
                    thread_id: "thread-1".into(),
                    goal_id: "goal-1".into(),
                },
                LoopEvent::MergeStateUpdated(pending_view()),
            ]
        );
    }

    fn bound_view() -> MergeApprovalView {
        let mut view = pending_view();
        view.binding = Some(MergeBinding {
            token_id: "token-1".into(),
            repo: "turtton/evorch".into(),
            pr_number: 65,
            head_sha: "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0".into(),
            snapshot: event_bus::GateSnapshot {
                repo: "turtton/evorch".into(),
                pr_number: 65,
                base_ref: "main".into(),
                head_sha: "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0".into(),
                ci: event_bus::CiState::Green,
                criteria_round: 1,
                review_round: 1,
                reviewer_run_id: "run-review-1".into(),
            },
        });
        view
    }

    #[test]
    fn reducer_binds_merge_view_on_merge_approval_requested() {
        let mut view = pending_view();
        let mut status = LoopStatusView::default();
        let binding = MergeBinding {
            token_id: "token-1".into(),
            repo: "turtton/evorch".into(),
            pr_number: 101,
            head_sha: "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0".into(),
            snapshot: event_bus::GateSnapshot {
                repo: "turtton/evorch".into(),
                pr_number: 101,
                base_ref: "main".into(),
                head_sha: "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0".into(),
                ci: event_bus::CiState::Green,
                criteria_round: 1,
                review_round: 1,
                reviewer_run_id: "run-review-1".into(),
            },
        };

        apply_orchestrator_event(
            &mut view,
            &mut status,
            &OrchestratorEvent::MergeApprovalRequested {
                goal_id: "goal-1".into(),
                binding: binding.clone(),
            },
        );

        assert_eq!(view.binding, Some(binding));
        assert!(!view.gate.is_empty());
        assert_eq!(view.blocked, None);
        assert_eq!(status.goal_id.as_deref(), Some("goal-1"));
    }

    #[test]
    fn reducer_marks_blocked_on_rounds_exhausted_and_disables_approve() {
        let mut view = bound_view();
        let mut status = LoopStatusView::default();

        apply_orchestrator_event(
            &mut view,
            &mut status,
            &OrchestratorEvent::FinishRejected {
                goal_id: "goal-1".into(),
                run_id: "run-root-1".into(),
                rejections: vec![GateRejection::ReviewRoundsExhausted { rounds: 3 }],
            },
        );

        assert_eq!(view.blocked.as_deref(), Some("review_rounds_exhausted"));
        assert_eq!(
            status.last_rejections,
            vec!["review_rounds_exhausted".to_string()]
        );

        apply_orchestrator_event(
            &mut view,
            &mut status,
            &OrchestratorEvent::GoalStateChanged {
                goal_id: "goal-1".into(),
                from: GoalState::Active,
                to: GoalState::Blocked,
                reason: "review rounds exhausted".into(),
            },
        );

        assert_eq!(status.state, Some(GoalState::Blocked));
        assert_eq!(view.blocked.as_deref(), Some("review rounds exhausted"));
    }

    #[test]
    fn decide_without_binding_returns_none() {
        let mut model = MergeApprovalModel {
            view: pending_view(),
        };

        assert!(model.decide(MergeDecision::Approve).is_none());
        assert_eq!(model.view.resolution, None);
    }

    #[test]
    fn merge_model_emits_decision_exactly_once() {
        let mut model = MergeApprovalModel { view: bound_view() };

        let first = model.decide(MergeDecision::Approve);
        let second = model.decide(MergeDecision::Approve);

        let Some(WorkbenchCommand::DecideMerge(command)) = first else {
            panic!("first decide must issue a command");
        };
        assert_eq!(command.token_id.as_deref(), Some("token-1"));
        assert!(second.is_none());
    }

    #[test]
    fn reject_requires_reason() {
        let mut model = MergeApprovalModel { view: bound_view() };

        assert!(
            model
                .decide(MergeDecision::Reject {
                    reason: String::new(),
                })
                .is_none()
        );
        assert!(model.view.resolution.is_none());
    }

    #[test]
    fn recording_sink_records_in_order() {
        let mut sink = RecordingSink::default();
        let goal = WorkbenchCommand::SubmitGoal(GoalSubmission {
            project_id: "evorch".into(),
            thread_id: "thread-1".into(),
            goal: "goal".into(),
            references: Vec::new(),
            constraints: Vec::new(),
        });
        let merge = WorkbenchCommand::DecideMerge(MergeCommand {
            thread_id: "thread-1".into(),
            pr: None,
            token_id: None,
            decision: MergeDecision::Approve,
        });

        assert!(sink.submit(goal.clone()).is_empty());
        assert!(sink.submit(merge.clone()).is_empty());
        assert_eq!(sink.issued, vec![goal, merge]);
    }
}
