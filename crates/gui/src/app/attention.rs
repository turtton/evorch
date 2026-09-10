use std::collections::BTreeMap;

use egui::Color32;
use event_bus::AgentRunPhase;
use workspace_ui::{PanelId, PanelKind, ThreadRunPhase};

use super::WorkbenchState;
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
    pub phases: &'a BTreeMap<String, ThreadRunPhase>,
    pub tasks_rows: &'a [TaskRow],
}

pub(super) fn attention_for(
    kind: PanelKind,
    target: Option<&str>,
    inputs: &AttentionInputs,
) -> PaneAttention {
    match kind {
        PanelKind::Agents | PanelKind::Tasks => inputs
            .tasks_rows
            .iter()
            .map(|row| agent_run_attention(row.status))
            .fold(PaneAttention::None, PaneAttention::max),
        PanelKind::AgentTranscript => target
            .and_then(|run_id| inputs.phases.get(run_id))
            .map_or(PaneAttention::None, |phase| thread_phase_attention(*phase)),
        PanelKind::Sidebar | PanelKind::Agent | PanelKind::Diff | PanelKind::Terminal | PanelKind::Memory => {
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
    fn waiting_transcript_is_warning() {
        let phases = BTreeMap::from([("run-1".to_owned(), ThreadRunPhase::Waiting)]);
        let inputs = AttentionInputs {
            phases: &phases,
            tasks_rows: &[],
        };

        assert_eq!(
            attention_for(PanelKind::AgentTranscript, Some("run-1"), &inputs),
            PaneAttention::Warning
        );
        assert_eq!(PaneAttention::Warning.color(), Some(WARNING_FG));
    }

    #[test]
    fn failed_transcript_is_error() {
        let phases = BTreeMap::from([("run-1".to_owned(), ThreadRunPhase::Error)]);
        let inputs = AttentionInputs {
            phases: &phases,
            tasks_rows: &[],
        };

        assert_eq!(
            attention_for(PanelKind::AgentTranscript, Some("run-1"), &inputs),
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
        let inputs = AttentionInputs {
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
        let rows = [task_row(AgentRunPhase::Error)];
        let inputs = AttentionInputs {
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
