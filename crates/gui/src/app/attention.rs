use std::collections::BTreeMap;

use egui::Color32;
use event_bus::{AgentRunPhase, GoalState};
use workspace_ui::{PanelId, PanelKind, ThreadRunPhase};

use super::WorkbenchState;
use crate::model::commands::{LoopStatusView, MergeApprovalView};
use crate::model::tasks::{AgentRunSource, TaskRow};
use crate::theme::tokens::{ERROR_FG, INFO, WARNING_FG};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum PaneAttention {
    None,
    Info,
    Warning,
    Error,
}

impl PaneAttention {
    pub(super) const fn color(self) -> Option<Color32> {
        match self {
            PaneAttention::None => None,
            PaneAttention::Info => Some(INFO),
            PaneAttention::Warning => Some(WARNING_FG),
            PaneAttention::Error => Some(ERROR_FG),
        }
    }
}

pub(super) struct AttentionInputs<'a> {
    pub merge: &'a MergeApprovalView,
    pub loop_status: &'a LoopStatusView,
    pub phases: &'a BTreeMap<String, ThreadRunPhase>,
    pub tasks_rows: &'a [TaskRow],
}

pub(super) fn attention_for(
    kind: PanelKind,
    target: Option<&str>,
    inputs: &AttentionInputs,
) -> PaneAttention {
    match kind {
        PanelKind::MergeApproval => {
            if inputs.merge.blocked.is_some() {
                PaneAttention::Error
            } else if inputs.merge.pr.is_some() && inputs.merge.resolution.is_none() {
                PaneAttention::Warning
            } else {
                PaneAttention::None
            }
        }
        PanelKind::Goal => {
            if inputs.merge.blocked.is_some() {
                PaneAttention::Error
            } else if inputs.loop_status.state == Some(GoalState::Paused) {
                PaneAttention::Warning
            } else if inputs.loop_status.goal_id.is_some()
                && inputs.loop_status.state == Some(GoalState::Active)
            {
                PaneAttention::Info
            } else {
                PaneAttention::None
            }
        }
        PanelKind::Agents | PanelKind::Tasks => inputs
            .tasks_rows
            .iter()
            .map(|row| agent_run_attention(row.status))
            .fold(PaneAttention::None, PaneAttention::max),
        PanelKind::AgentTranscript => target
            .and_then(|run_id| inputs.phases.get(run_id))
            .map_or(PaneAttention::None, |phase| thread_phase_attention(*phase)),
        PanelKind::Sidebar | PanelKind::Agent | PanelKind::Diff | PanelKind::Terminal => {
            PaneAttention::None
        }
    }
}

const fn agent_run_attention(phase: AgentRunPhase) -> PaneAttention {
    match phase {
        AgentRunPhase::Pending | AgentRunPhase::Done => PaneAttention::None,
        AgentRunPhase::Running => PaneAttention::Info,
        AgentRunPhase::Waiting => PaneAttention::Warning,
        AgentRunPhase::Error => PaneAttention::Error,
    }
}

const fn thread_phase_attention(phase: ThreadRunPhase) -> PaneAttention {
    match phase {
        ThreadRunPhase::Pending | ThreadRunPhase::Done => PaneAttention::None,
        ThreadRunPhase::Running => PaneAttention::Info,
        ThreadRunPhase::Waiting => PaneAttention::Warning,
        ThreadRunPhase::Error => PaneAttention::Error,
    }
}

impl<S: AgentRunSource> WorkbenchState<S> {
    pub fn pane_attention(&self, panel_id: &PanelId) -> Option<Color32> {
        let panel = self.panels.get(panel_id)?;
        attention_for(
            panel.kind,
            panel.target.as_deref(),
            &AttentionInputs {
                merge: &self.merge.view,
                loop_status: &self.loop_status,
                phases: &self.phases,
                tasks_rows: self.tasks.rows(),
            },
        )
        .color()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::commands::{CiStatus, PrRef, ReviewerStatus};

    fn merge_view(
        pr: Option<PrRef>,
        resolution: Option<crate::model::commands::MergeDecision>,
        blocked: Option<String>,
    ) -> MergeApprovalView {
        MergeApprovalView {
            pr,
            ci: CiStatus::Unknown,
            reviewer: ReviewerStatus::Unknown,
            diff_summary: None,
            resolution,
            binding: None,
            gate: Vec::new(),
            blocked,
        }
    }

    fn demo_pr() -> PrRef {
        PrRef {
            number: 65,
            title: "Workbench restructure".to_owned(),
            url: "https://github.com/turtton/evorch/pull/65".to_owned(),
        }
    }

    fn task_row(status: AgentRunPhase) -> TaskRow {
        TaskRow {
            run_id: runtime::RunId::new(1),
            name: "orchestrator".to_owned(),
            role: "orchestrator".to_owned(),
            status,
            model: "demo".to_owned(),
        }
    }

    #[test]
    fn merge_pending_review_is_warning() {
        // Given: a bound, unresolved PR with no blocker
        let merge = merge_view(Some(demo_pr()), None, None);
        let inputs = AttentionInputs {
            merge: &merge,
            loop_status: &LoopStatusView::default(),
            phases: &BTreeMap::new(),
            tasks_rows: &[],
        };

        // Then: the merge tab requests attention as a warning
        assert_eq!(
            attention_for(PanelKind::MergeApproval, None, &inputs),
            PaneAttention::Warning
        );
        assert_eq!(PaneAttention::Warning.color(), Some(WARNING_FG));
    }

    #[test]
    fn merge_blocked_is_error_even_with_pending_pr() {
        // Given: a pending PR whose goal is blocked
        let merge = merge_view(Some(demo_pr()), None, Some("goal blocked".to_owned()));
        let inputs = AttentionInputs {
            merge: &merge,
            loop_status: &LoopStatusView::default(),
            phases: &BTreeMap::new(),
            tasks_rows: &[],
        };

        // Then: the blocker outranks the pending review
        assert_eq!(
            attention_for(PanelKind::MergeApproval, None, &inputs),
            PaneAttention::Error
        );
        assert_eq!(PaneAttention::Error.color(), Some(ERROR_FG));
    }

    #[test]
    fn running_agents_mark_agents_tab_as_info() {
        // Given: one running and one done run
        let rows = [
            task_row(AgentRunPhase::Running),
            task_row(AgentRunPhase::Done),
        ];
        let merge = merge_view(None, None, None);
        let inputs = AttentionInputs {
            merge: &merge,
            loop_status: &LoopStatusView::default(),
            phases: &BTreeMap::new(),
            tasks_rows: &rows,
        };

        // Then: both agent-list tabs are marked as info
        for kind in [PanelKind::Agents, PanelKind::Tasks] {
            assert_eq!(
                attention_for(kind, None, &inputs),
                PaneAttention::Info,
                "{kind:?}"
            );
        }
        assert_eq!(PaneAttention::Info.color(), Some(INFO));
    }

    #[test]
    fn sidebar_tab_has_no_attention() {
        // Given: inputs that would flag every other tab
        let merge = merge_view(Some(demo_pr()), None, Some("goal blocked".to_owned()));
        let rows = [task_row(AgentRunPhase::Error)];
        let inputs = AttentionInputs {
            merge: &merge,
            loop_status: &LoopStatusView {
                state: Some(GoalState::Paused),
                ..LoopStatusView::default()
            },
            phases: &BTreeMap::new(),
            tasks_rows: &rows,
        };

        // Then: navigation and static panes stay quiet
        for kind in [
            PanelKind::Sidebar,
            PanelKind::Agent,
            PanelKind::Diff,
            PanelKind::Terminal,
        ] {
            assert_eq!(
                attention_for(kind, None, &inputs),
                PaneAttention::None,
                "{kind:?}"
            );
        }
        assert_eq!(PaneAttention::None.color(), None);
    }
}
