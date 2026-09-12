use std::collections::BTreeMap;

#[path = "../model/ack.rs"]
pub mod ack;

use egui::Color32;
use event_bus::AgentRunPhase;
use workspace_ui::{PanelId, PanelKind, ThreadRunPhase};

use super::WorkbenchState;
use crate::model::tasks::{AgentRunSource, TaskRow};
use crate::theme::tokens::{ERROR_FG, INFO};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum PaneAttention {
    None,
    Info,
    Error,
}

impl PaneAttention {
    pub(super) const fn color(self) -> Option<Color32> {
        match self {
            PaneAttention::None => None,
            PaneAttention::Info => Some(INFO),
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
        PanelKind::Sidebar
        | PanelKind::Notifications
        | PanelKind::Agent
        | PanelKind::Diff
        | PanelKind::Terminal
        | PanelKind::Memory
        | PanelKind::Arena => PaneAttention::None,
    }
}

const fn agent_run_attention(phase: AgentRunPhase) -> PaneAttention {
    match phase {
        AgentRunPhase::Pending | AgentRunPhase::Running => PaneAttention::None,
        AgentRunPhase::Done | AgentRunPhase::Waiting => PaneAttention::Info,
        AgentRunPhase::Error => PaneAttention::Error,
    }
}

const fn thread_phase_attention(phase: ThreadRunPhase) -> PaneAttention {
    match phase {
        ThreadRunPhase::Pending | ThreadRunPhase::Running => PaneAttention::None,
        ThreadRunPhase::Done | ThreadRunPhase::Waiting => PaneAttention::Info,
        ThreadRunPhase::Error => PaneAttention::Error,
    }
}

impl<S: AgentRunSource> WorkbenchState<S> {
    pub(super) fn observe_attention(&mut self) {
        let mut observed = BTreeMap::new();
        for (id, panel) in &self.panels {
            let runs: Vec<(String, ThreadRunPhase)> = match panel.kind {
                PanelKind::Agent => match &self.focus {
                    super::ConversationFocus::Thread => self
                        .sidebar
                        .active_thread
                        .as_ref()
                        .and_then(|id| self.sidebar.threads.iter().find(|thread| &thread.id == id))
                        .and_then(|thread| thread.run_ids.last())
                        .and_then(|run| self.phases.get(run).map(|phase| (run.clone(), *phase)))
                        .into_iter()
                        .collect(),
                    super::ConversationFocus::Agent(run) => self
                        .phases
                        .get(run)
                        .map(|phase| (run.clone(), *phase))
                        .into_iter()
                        .collect(),
                },
                PanelKind::AgentTranscript => panel
                    .target
                    .as_ref()
                    .and_then(|run| self.phases.get(run).map(|phase| (run.clone(), *phase)))
                    .into_iter()
                    .collect(),
                PanelKind::Agents | PanelKind::Tasks => self
                    .tasks
                    .rows()
                    .iter()
                    .map(|row| {
                        let phase = match row.status {
                            AgentRunPhase::Pending => ThreadRunPhase::Pending,
                            AgentRunPhase::Running => ThreadRunPhase::Running,
                            AgentRunPhase::Waiting => ThreadRunPhase::Waiting,
                            AgentRunPhase::Done => ThreadRunPhase::Done,
                            AgentRunPhase::Error => ThreadRunPhase::Error,
                        };
                        (row.run_id.to_string(), phase)
                    })
                    .collect(),
                PanelKind::Sidebar
                | PanelKind::Notifications
                | PanelKind::Diff
                | PanelKind::Terminal
                | PanelKind::Memory
                | PanelKind::Arena => Vec::new(),
            };
            for (run, phase) in runs {
                observed.insert((id.clone(), run), phase);
            }
        }
        self.attention_acks
            .retain(|key, _| observed.contains_key(key));
        for (key, phase) in observed {
            let ack = self
                .attention_acks
                .entry(key)
                .or_insert_with(Self::new_attention_ack);
            if ack.observe(phase).is_err() {
                // A fresh lifetime invalidates outstanding tokens if the sequence is exhausted.
                *ack = Self::new_attention_ack();
                if ack.observe(phase).is_err() {
                    continue;
                }
            }
        }
    }

    /// Creates state for one pane/thread lifetime; the renderer owns and retains it.
    pub fn new_attention_ack() -> ack::AttentionAck {
        ack::AttentionAck::default()
    }

    pub fn pane_attention(&self, panel_id: &PanelId) -> Option<Color32> {
        if self.attention_acks.keys().any(|(id, _)| id == panel_id) {
            return acknowledged_attention(&self.attention_acks, panel_id).color();
        }
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

pub(super) fn acknowledged_attention(
    acks: &BTreeMap<(PanelId, String), ack::AttentionAck>,
    panel: &PanelId,
) -> PaneAttention {
    acks.iter()
        .filter(|((id, _), ack)| id == panel && ack.is_unread())
        .map(|(_, ack)| thread_phase_attention(ack.phase()))
        .fold(PaneAttention::None, PaneAttention::max)
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
    fn waiting_transcript_is_info() {
        let phases = BTreeMap::from([("run-1".to_owned(), ThreadRunPhase::Waiting)]);
        let inputs = AttentionInputs {
            phases: &phases,
            tasks_rows: &[],
        };

        assert_eq!(
            attention_for(PanelKind::AgentTranscript, Some("run-1"), &inputs),
            PaneAttention::Info
        );
        assert_eq!(PaneAttention::Info.color(), Some(INFO));
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
    fn running_agents_do_not_request_unread_emphasis() {
        // Given: one running and one done run
        let rows = [
            task_row(AgentRunPhase::Running),
            task_row(AgentRunPhase::Pending),
        ];
        let inputs = AttentionInputs {
            phases: &BTreeMap::new(),
            tasks_rows: &rows,
        };

        // Then: both agent-list tabs are marked as info
        for kind in [PanelKind::Agents, PanelKind::Tasks] {
            assert_eq!(
                attention_for(kind, None, &inputs),
                PaneAttention::None,
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
