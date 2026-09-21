use super::{AgentRunSource, ConversationFocus, PanelId, WorkbenchTabViewer};
use crate::panes::agent::{
    AgentIdentity, AgentPaneAction, ConversationContext, agent_pane_with_repo_root,
};

impl<S: AgentRunSource> WorkbenchTabViewer<'_, S> {
    pub(super) fn agent_tab_ui(&mut self, ui: &mut egui::Ui, tab: &PanelId) {
        let (transcript, identity) = match self.focus {
            ConversationFocus::Thread => (self.transcripts.thread(), None),
            ConversationFocus::Agent(run_id) => {
                let transcript = self
                    .transcripts
                    .run(run_id)
                    .unwrap_or_else(|| self.transcripts.thread());
                let row = self
                    .tasks
                    .rows()
                    .iter()
                    .find(|row| row.run_id.to_string() == *run_id);
                (
                    transcript,
                    Some(AgentIdentity {
                        run_id,
                        name: row.map(|row| row.name.as_str()),
                        role: row.map(|row| row.role.as_str()),
                        ledger: self.ledger.entries(run_id),
                    }),
                )
            }
        };
        let active_thread = self
            .sidebar
            .active_thread
            .as_ref()
            .and_then(|id| self.sidebar.threads.iter().find(|thread| &thread.id == id));
        let ctx = ConversationContext {
            sandbox_picker: self.sandbox_picker,
            task_rows: self.tasks.rows(),
            phase_unread: self
                .attention_acks
                .iter()
                .any(|((id, _), ack)| id == tab && ack.is_unread()),
            has_project: self.sidebar.selected_project.is_some(),
            active_thread_title: active_thread.map(|thread| thread.title.as_str()),
            thread_metrics: active_thread.map(|thread| {
                let mut metrics = self.telemetry.thread_metrics(&thread.run_ids);
                if let ConversationFocus::Agent(run_id) = self.focus {
                    metrics.context_pressure = self
                        .telemetry
                        .row(run_id)
                        .and_then(crate::model::telemetry::TelemetryRow::context_pressure);
                }
                metrics
            }),
            phase: self
                .attention_acks
                .iter()
                .find(|((id, _), _)| id == tab)
                .map(|(_, ack)| ack.phase()),
            next_thread_title: format!("thread-{}", self.sidebar.threads.len() + 1),
            model_picker: crate::panes::model_picker::ModelPickerContext {
                profiles: self.profiles,
                preference: active_thread.and_then(|thread| thread.model_preference.as_ref()),
                enabled: active_thread.is_some(),
            },
        };
        if let Some(action) = agent_pane_with_repo_root(
            ui,
            transcript,
            identity,
            ctx,
            self.composer,
            self.picker_state,
            self.repo_root,
        ) {
            match action {
                AgentPaneAction::Agents(a) => *self.agents_action = Some(a),
                AgentPaneAction::Sidebar(a) => *self.sidebar_action = Some(a),
                AgentPaneAction::FocusPanel(id) => *self.focus_request = Some(id),
                AgentPaneAction::Composer(a) => *self.composer_action = Some(a),
                AgentPaneAction::ModelPreference(preference) => {
                    *self.preference_action = Some(preference)
                }
            }
        }
    }
}
