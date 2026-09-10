use super::WorkbenchState;
use crate::model::commands::{ChatSubmission, WorkbenchCommand};
use crate::model::composer::{ComposerInput, ProviderStatus, parse_input};
use crate::model::tasks::AgentRunSource;
use crate::model::transcript::TranscriptEntry;

impl<S: AgentRunSource> WorkbenchState<S> {
    pub fn load_external_commands(&mut self, executable: std::path::PathBuf) {
        match crate::model::composer::SlashCommandRegistry::discover(executable) {
            Ok(registry) => self.composer.registry = registry,
            Err(error) => self.push_notice(error),
        }
    }
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
        self.refresh_image_capability();
        if !self.thread_writable() {
            self.push_notice("Read-only attach: explicitly Start or Claim before sending.");
            return;
        }
        let raw = self.composer.input.clone();
        let parsed = if raw.trim().is_empty() && !self.composer.attachments.is_empty() {
            ComposerInput::Chat("")
        } else {
            parse_input(&raw)
        };
        match parsed {
            ComposerInput::Empty => {}
            ComposerInput::Chat(text) => {
                if let Some(warning) = self.composer.image_warning() {
                    self.push_notice(warning);
                    return;
                }
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
                            images: self
                                .composer
                                .attachments
                                .iter()
                                .map(|image| runtime::DelegateImage {
                                    media_type: image.media_type.clone(),
                                    data: image.data.clone(),
                                })
                                .collect(),
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
                        self.composer.attachments.clear();
                    }
                }
            }
            ComposerInput::Command { spec, args } => match spec.name {
                "undo" | "redo" => {
                    if let Some(thread_id) = self.sidebar.active_thread.as_ref() {
                        self.submit_command(WorkbenchCommand::RestoreSnapshot {
                            thread_id: thread_id.to_string(),
                            redo: spec.name == "redo",
                        });
                        self.composer.input.clear();
                    }
                }
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
                    self.push_notice(self.composer.registry.help_text());
                    self.composer.input.clear();
                }
                name => self.push_notice(format!("unknown command /{name} — type /help")),
            },
            ComposerInput::UnknownCommand { name } => {
                if self.composer.registry.parse(&raw).is_some() {
                    let root = self
                        .sidebar
                        .projects
                        .iter()
                        .find(|project| Some(&project.id) == self.sidebar.selected_project.as_ref())
                        .map(|project| project.repo_root.clone());
                    match root {
                        Some(root) => match self.composer.registry.execute(&raw, &root) {
                            Ok(output) => {
                                self.push_notice(output);
                                self.composer.input.clear();
                            }
                            Err(error) => self.push_notice(error),
                        },
                        None => self.push_notice("Select a project first"),
                    }
                } else {
                    self.push_notice(format!("unknown command /{name} — type /help"));
                }
            }
        }
    }

    pub(super) fn push_notice(&mut self, text: impl Into<String>) {
        self.transcripts
            .push_thread(TranscriptEntry::Notice { text: text.into() });
    }

    pub(super) fn refresh_image_capability(&mut self) {
        let Some((_, model)) = self.production_model.as_ref() else {
            return;
        };
        let preference = self
            .sidebar
            .threads
            .iter()
            .find(|thread| Some(&thread.id) == self.sidebar.active_thread.as_ref())
            .and_then(|thread| thread.model_preference.as_ref());
        let profiles = model.available_profiles();
        let selected = preference
            .and_then(|preference| {
                preference.model.clone().or_else(|| {
                    profiles
                        .iter()
                        .find(|profile| profile.name == preference.profile)
                        .and_then(|profile| profile.default_model.clone())
                })
            })
            .unwrap_or_else(|| {
                runtime::AgentModel::selected_model(model.as_ref(), runtime::Role::Worker)
            });
        self.composer.image_input_supported = self
            .provider_settings
            .catalog
            .catalog
            .as_ref()
            .and_then(|catalog| catalog.find_unique_model(&selected))
            .is_some_and(|metadata| {
                metadata
                    .modalities
                    .input
                    .iter()
                    .any(|value| value == "image")
            });
    }
}
