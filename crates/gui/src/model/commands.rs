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
    pub decision: MergeDecision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkbenchCommand {
    SubmitGoal(GoalSubmission),
    DecideMerge(MergeCommand),
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
            WorkbenchCommand::DecideMerge(command) => vec![
                LoopEvent::MergeResolved {
                    thread_id: command.thread_id,
                    decision: command.decision.clone(),
                },
                LoopEvent::MergeStateUpdated(MergeApprovalView {
                    pr: command.pr.or_else(|| Self::fixture_view(None).pr),
                    resolution: Some(command.decision),
                    ..Self::fixture_view(None)
                }),
            ],
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
        if self.view.resolution.is_some()
            || matches!(&decision, MergeDecision::Reject { reason } if reason.trim().is_empty())
        {
            return None;
        }
        self.view.resolution = Some(decision.clone());
        Some(WorkbenchCommand::DecideMerge(MergeCommand {
            thread_id: String::new(),
            pr: self.view.pr.clone(),
            decision,
        }))
    }
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

    #[test]
    fn merge_model_emits_decision_exactly_once() {
        let mut model = MergeApprovalModel {
            view: pending_view(),
        };

        let first = model.decide(MergeDecision::Approve);
        let second = model.decide(MergeDecision::Approve);

        assert!(matches!(first, Some(WorkbenchCommand::DecideMerge(_))));
        assert!(second.is_none());
    }

    #[test]
    fn reject_requires_reason() {
        let mut model = MergeApprovalModel {
            view: pending_view(),
        };

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
            decision: MergeDecision::Approve,
        });

        assert!(sink.submit(goal.clone()).is_empty());
        assert!(sink.submit(merge.clone()).is_empty());
        assert_eq!(sink.issued, vec![goal, merge]);
    }
}
