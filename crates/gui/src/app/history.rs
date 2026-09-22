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
        self.transcripts = crate::model::transcript_registry::TranscriptRegistry::new();
        for thread in &self.sidebar.threads {
            for run in &thread.run_ids {
                self.transcripts.bind_run(run, &thread.id.to_string());
            }
        }
        let mut messages = self.history.clone();
        messages.sort_by_key(|message| message.at);
        let mut messages = messages.into_iter().peekable();
        self.telemetry = crate::model::telemetry::TelemetryOverlay::new();
        let replay_now = std::time::Instant::now();
        let replay_end = events.last().map(|stored| stored.event.meta.wall_clock);
        for stored in events {
            while messages
                .peek()
                .is_some_and(|message| message.at <= stored.event.meta.wall_clock)
            {
                if let Some(message) = messages.next() {
                    self.restore_user_message(message);
                }
            }
            self.bind_goal_event(&stored.event);
            let at = replay_end
                .and_then(|end| end.duration_since(stored.event.meta.wall_clock).ok())
                .and_then(|age| replay_now.checked_sub(age))
                .unwrap_or(replay_now);
            self.telemetry.apply_event_at(&stored.event, at);
            self.transcripts.select_thread(None);
            self.apply_conversation_event(&stored.event);
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
