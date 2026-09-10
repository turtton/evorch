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

    pub(super) fn bind_thread_run(&mut self, thread_id: &str, run_id: &str) {
        if let Some(thread) = self
            .sidebar
            .threads
            .iter_mut()
            .find(|thread| thread.id.to_string() == thread_id)
        {
            if !thread.run_ids.iter().any(|run| run == run_id) {
                thread.run_ids.push(run_id.into());
            }
            self.transcripts.bind_run(run_id, thread_id);
            self.save_sidebar();
        }
    }

    pub fn restore_history(&mut self, db: &storage::Database) -> Result<(), storage::StorageError> {
        let events = db.events_all_ordered()?;
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
        for stored in events {
            while messages
                .peek()
                .is_some_and(|message| message.at <= stored.event.meta.wall_clock)
            {
                if let Some(message) = messages.next() {
                    self.restore_user_message(message);
                }
            }
            if let event_bus::EventKind::Lifecycle(event_bus::LifecycleEvent::AgentRunStarted {
                run_id,
                parent_run_id,
                agent_name,
                ..
            }) = &stored.event.kind
            {
                let owner = agent_name
                    .strip_prefix("chat:")
                    .map(str::to_owned)
                    .or_else(|| {
                        self.sidebar
                            .threads
                            .iter()
                            .find(|thread| {
                                thread.run_ids.contains(run_id)
                                    || parent_run_id
                                        .as_ref()
                                        .is_some_and(|parent| thread.run_ids.contains(parent))
                            })
                            .map(|thread| thread.id.to_string())
                    });
                if let Some(owner) = owner {
                    self.bind_thread_run(&owner, run_id);
                }
            }
            self.transcripts.select_thread(None);
            self.transcripts.apply(&stored.event);
        }
        for message in messages {
            self.restore_user_message(message);
        }
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
