use super::WorkbenchState;
use crate::model::commands::{ChatSubmission, WorkbenchCommand};
use crate::model::composer::{ComposerInput, ProviderStatus, help_text, parse_input};
use crate::model::tasks::AgentRunSource;
use crate::model::transcript::TranscriptEntry;

impl<S: AgentRunSource> WorkbenchState<S> {
    pub fn cancel_chat(&mut self) {
        let Some(thread_id) = self.sidebar.active_thread.as_ref() else {
            return;
        };
        self.submit_command(WorkbenchCommand::CancelChat {
            thread_id: thread_id.to_string(),
        });
    }

    pub fn available_profiles(&self) -> Vec<runtime::compose::ProfileSummary> {
        self.production_model
            .as_ref()
            .map(|(_, model)| model.available_profiles())
            .unwrap_or_default()
    }

    pub fn set_thread_model_preference(
        &mut self,
        preference: Option<workspace_ui::ModelPreference>,
    ) {
        if let Some(thread) = self
            .sidebar
            .threads
            .iter_mut()
            .find(|thread| Some(&thread.id) == self.sidebar.active_thread.as_ref())
        {
            thread.model_preference = preference;
            self.save_sidebar();
        }
    }

    pub fn submit_composer(&mut self) {
        if !self.thread_writable() {
            self.push_notice("Read-only attach: explicitly Start or Claim before sending.");
            return;
        }
        let raw = self.composer.input.clone();
        match parse_input(&raw) {
            ComposerInput::Empty => {}
            ComposerInput::Chat(text) => {
                let (Some(_), Some(thread_id)) = (
                    self.sidebar.selected_project.as_ref(),
                    self.sidebar.active_thread.as_ref(),
                ) else {
                    self.push_notice("Select or start a thread first");
                    return;
                };
                match &self.provider_status {
                    ProviderStatus::NotConfigured { guidance } => {
                        self.push_notice(guidance.clone());
                    }
                    ProviderStatus::Configured => {
                        let submission = ChatSubmission {
                            thread_id: thread_id.to_string(),
                            text: text.into(),
                            model_preference: self
                                .sidebar
                                .threads
                                .iter()
                                .find(|thread| &thread.id == thread_id)
                                .and_then(|thread| thread.model_preference.as_ref())
                                .map(|preference| runtime::ModelPreference {
                                    profile: preference.profile.clone(),
                                    model: preference.model.clone(),
                                }),
                        };
                        self.history.push(super::history::UserMessage {
                            thread_id: submission.thread_id.clone(),
                            text: text.into(),
                            at: std::time::SystemTime::now(),
                        });
                        self.transcripts
                            .push_thread(TranscriptEntry::UserMessage { text: text.into() });
                        self.save_sidebar();
                        self.submit_command(WorkbenchCommand::SendChat(submission));
                        self.composer.input.clear();
                    }
                }
            }
            ComposerInput::Command { spec, args } => match spec.name {
                "goal" => {
                    if args.is_empty() {
                        self.push_notice("usage: /goal <text>");
                    } else {
                        self.goal_form.goal = args.into();
                        self.submit_goal();
                        self.composer.input.clear();
                    }
                }
                "help" => {
                    self.push_notice(help_text());
                    self.composer.input.clear();
                }
                name => self.push_notice(format!("unknown command /{name} — type /help")),
            },
            ComposerInput::UnknownCommand { name } => {
                self.push_notice(format!("unknown command /{name} — type /help"));
            }
        }
    }

    pub(super) fn push_notice(&mut self, text: impl Into<String>) {
        self.transcripts
            .push_thread(TranscriptEntry::Notice { text: text.into() });
    }
}
