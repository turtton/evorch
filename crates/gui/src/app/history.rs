use super::WorkbenchState;
use crate::model::tasks::AgentRunSource;
use crate::model::transcript::TranscriptEntry;
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct UserMessage {
    pub thread_id: String,
    pub text: String,
    pub at: std::time::SystemTime,
}

#[derive(Serialize, Deserialize)]
struct HistorySidebar {
    #[serde(flatten)]
    sidebar: workspace_ui::SidebarState,
    #[serde(default)]
    user_messages: Vec<UserMessage>,
}

impl<S: AgentRunSource> WorkbenchState<S> {
    pub(super) fn write_history_sidebar(
        &self,
        path: &std::path::Path,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.sidebar.validate()?;
        let saved = HistorySidebar {
            sidebar: self.sidebar.clone(),
            user_messages: self.history.clone(),
        };
        std::fs::write(path, serde_json::to_vec_pretty(&saved)?)?;
        Ok(())
    }

    pub fn restore_history(&mut self, db: &storage::Database) -> Result<(), storage::StorageError> {
        let events = db.events_all_ordered()?;
        // A legacy event subscriber could drop stream deltas under load. The
        // context snapshot still contains the complete accepted response.
        // Repair the final waiting turn at its original position in the replay.
        let mut last_waiting = std::collections::BTreeMap::new();
        let mut completed_at_waiting = std::collections::BTreeMap::new();
        let mut completed_since_request = std::collections::BTreeMap::new();
        for (index, stored) in events.iter().enumerate() {
            match &stored.event.kind {
                event_bus::EventKind::Provider(event_bus::ProviderEvent::RequestStarted {
                    run_id: Some(run_id),
                    ..
                }) => {
                    completed_since_request.insert(run_id.clone(), false);
                }
                event_bus::EventKind::Message(event_bus::MessageEvent::MessageCompleted {
                    run_id,
                    ..
                }) => {
                    completed_since_request.insert(run_id.clone(), true);
                }
                event_bus::EventKind::Lifecycle(
                    event_bus::LifecycleEvent::AgentRunStateChanged {
                        run_id,
                        to: event_bus::AgentRunPhase::Waiting,
                        ..
                    },
                ) => {
                    last_waiting.insert(run_id.clone(), index);
                    completed_at_waiting.insert(
                        run_id.clone(),
                        completed_since_request
                            .get(run_id)
                            .copied()
                            .unwrap_or(false),
                    );
                }
                _ => {}
            }
        }
        self.user_questions = db
            .pending_user_questions()?
            .into_iter()
            .map(|q| (q.id.clone(), q))
            .collect();
        if let Some(path) = &self.sidebar_path {
            match std::fs::read(path) {
                Ok(bytes) => {
                    let saved: HistorySidebar = serde_json::from_slice(&bytes)
                        .map_err(|error| storage::StorageError::Serialization(error.to_string()))?;
                    self.history = saved.user_messages;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(storage::StorageError::Serialization(error.to_string())),
            }
        }
        // Older sidebar files did not record a thread's chat role. Infer it from
        // the first root chat run, so a restarted thread does not silently start
        // a different role with an empty conversation.
        let mut inferred_role = false;
        for thread in &mut self.sidebar.threads {
            if thread.chat_role.is_some() || thread.escalation_source_run_id.is_some() {
                continue;
            }
            for run_id in &thread.run_ids {
                let Some(record) = db.run_context(run_id)? else {
                    continue;
                };
                if record.parent_run_id.is_some()
                    || record.name != format!("chat:{}:{}", record.role, thread.id)
                {
                    continue;
                }
                thread.chat_role = match record.role.as_str() {
                    "Worker" => Some(workspace_ui::ThreadChatRole::Worker),
                    "Orchestrator" => Some(workspace_ui::ThreadChatRole::Orchestrator),
                    _ => None,
                };
                if thread.chat_role.is_some() {
                    inferred_role = true;
                    break;
                }
            }
        }
        if let Some(thread) = self
            .sidebar
            .threads
            .iter()
            .find(|thread| Some(&thread.id) == self.sidebar.active_thread.as_ref())
        {
            self.composer.role = if thread.escalation_source_run_id.is_some() {
                crate::model::composer::ComposerRole::Orchestrator
            } else {
                thread.chat_role.map(Into::into).unwrap_or_default()
            };
        }
        if inferred_role {
            self.save_sidebar();
        }
        self.transcripts = crate::model::transcript_registry::TranscriptRegistry::new();
        for thread in &self.sidebar.threads {
            for run in &thread.run_ids {
                self.transcripts.bind_run(run, &thread.id.to_string());
            }
        }
        let mut completed_snapshots = std::collections::BTreeMap::new();
        for thread in &self.sidebar.threads {
            for run_id in &thread.run_ids {
                if !last_waiting.contains_key(run_id) {
                    continue;
                }
                let Some(record) = db.run_context(run_id)? else {
                    continue;
                };
                if record.terminal_phase != "Checkpoint" {
                    continue;
                }
                let Ok(messages) =
                    serde_json::from_str::<Vec<providers::Message>>(&record.messages_json)
                else {
                    continue;
                };
                let Some(last) = messages.last().filter(|message| {
                    message.role == providers::Role::Assistant
                        && !message
                            .content
                            .iter()
                            .any(|block| matches!(block, providers::ContentBlock::ToolUse { .. }))
                }) else {
                    continue;
                };
                let text: String = last
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        providers::ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                if !text.is_empty() {
                    completed_snapshots.insert(run_id.clone(), text);
                }
            }
        }
        let mut messages = self.history.clone();
        messages.sort_by_key(|message| message.at);
        let mut messages = messages.into_iter().peekable();
        self.telemetry = crate::model::telemetry::TelemetryOverlay::new();
        let replay_now = std::time::Instant::now();
        let replay_end = events.last().map(|stored| stored.event.meta.wall_clock);
        for (index, stored) in events.into_iter().enumerate() {
            while messages
                .peek()
                .is_some_and(|message| message.at <= stored.event.meta.wall_clock)
            {
                if let Some(message) = messages.next() {
                    self.restore_user_message(message);
                }
            }
            let at = replay_end
                .and_then(|end| end.duration_since(stored.event.meta.wall_clock).ok())
                .and_then(|age| replay_now.checked_sub(age))
                .unwrap_or(replay_now);
            self.telemetry.apply_event_at(&stored.event, at);
            self.transcripts.select_thread(None);
            self.apply_conversation_event(&stored.event);
            self.bind_goal_event(&stored.event);
            if let event_bus::EventKind::Lifecycle(
                event_bus::LifecycleEvent::AgentRunStateChanged { run_id, .. },
            ) = &stored.event.kind
                && last_waiting.get(run_id) == Some(&index)
                && !completed_at_waiting.get(run_id).copied().unwrap_or(false)
                && let Some(text) = completed_snapshots.get(run_id)
            {
                self.apply_conversation_event(&event_bus::Event::new(
                    event_bus::MessageEvent::MessageCompleted {
                        run_id: run_id.clone(),
                        text: text.clone(),
                    },
                ));
            }
        }
        for message in messages {
            self.restore_user_message(message);
        }
        self.transcripts.finish_history();
        self.telemetry.finish_history();
        self.ledger.load_all(db.run_ledger_all()?);
        self.transcripts
            .select_thread(self.sidebar.active_thread.as_ref().map(ToString::to_string));
        Ok(())
    }

    fn restore_user_message(&mut self, message: UserMessage) {
        self.transcripts.select_thread(Some(message.thread_id));
        self.transcripts
            .push_thread(TranscriptEntry::UserMessage { text: message.text });
    }
}
