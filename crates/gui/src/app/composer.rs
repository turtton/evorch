use super::WorkbenchState;
use super::history::DeliveryStatus;
use crate::model::commands::{ChatContinuation, ChatSubmission, LoopEvent, WorkbenchCommand};
use crate::model::composer::{
    ComposerInput, ProviderStatus, mention_skills, withheld_skill_mentions,
};
use crate::model::project_bridge::repo_skills_admitted;
use crate::model::tasks::AgentRunSource;
use crate::model::transcript::TranscriptEntry;
use crate::panes::project_dialog::TRUST_LABEL;

/// Tells why the mentioned repository skills `names` went out as plain text
/// and where to trust the project so they are inlined.
fn untrusted_skills_notice(names: &[&str]) -> String {
    let mentions = names
        .iter()
        .map(|name| format!("@{name}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "Sent {mentions} as plain text — this project is not trusted, so its own skills are \
         not expanded. Enable \"{TRUST_LABEL}\" in Project settings to use them."
    )
}

impl<S: AgentRunSource> WorkbenchState<S> {
    /// Continue a thread's saved conversation, as `/continue` does. `automatic`
    /// marks a resume the runtime requested to deliver held subagent reports.
    pub(super) fn chat_continuation(
        &self,
        thread_id: &workspace_ui::ThreadId,
        automatic: bool,
    ) -> ChatContinuation {
        ChatContinuation {
            project_root: self
                .thread_project(thread_id)
                .map(|project| project.repo_root.clone()),
            thread_id: thread_id.to_string(),
            composer_role: self.composer.role,
            model_preference: self
                .sidebar
                .threads
                .iter()
                .find(|thread| &thread.id == thread_id)
                .and_then(|thread| thread.model_preference.as_ref())
                .map(|preference| runtime::ModelPreference {
                    profile: preference.profile.clone(),
                    model: preference.model.clone(),
                    reasoning_effort: preference.reasoning_effort.clone(),
                }),
            automatic,
        }
    }

    pub fn load_external_commands(&mut self, executable: std::path::PathBuf) {
        self.start_external(async move {
            crate::model::composer::SlashCommandRegistry::discover(executable)
                .await
                .map(super::external_commands::Outcome::Discovery)
        });
    }
    pub fn stop_chat(&mut self) {
        let Some(thread_id) = self.sidebar.active_thread.as_ref() else {
            return;
        };
        self.submit_command(WorkbenchCommand::StopChat {
            thread_id: thread_id.to_string(),
        });
    }

    pub fn deliver_follow_ups_next_turn(&mut self) {
        let Some(thread_id) = self.sidebar.active_thread.as_ref() else {
            return;
        };
        self.submit_command(WorkbenchCommand::DeliverFollowUpsNextTurn {
            thread_id: thread_id.to_string(),
        });
    }

    pub fn available_profiles(&self) -> Vec<runtime::compose::ProfileSummary> {
        self.active_model()
            .map(|model| model.available_profiles())
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
        self.flush_composer_draft();
        // A persisted conversation role is authoritative for chat and /continue,
        // even if an editor caller changed the transient composer model.
        if let Some(thread) = self
            .sidebar
            .threads
            .iter()
            .find(|thread| Some(&thread.id) == self.sidebar.active_thread.as_ref())
            && (thread.chat_role.is_some() || thread.escalation_source_run_id.is_some())
        {
            self.composer.restore_thread_role(thread);
        }
        if !self.sync_shell_cwd() {
            return;
        }
        self.refresh_image_capability();
        let raw = self.composer.input.clone();
        let parsed = if raw.trim().is_empty() && !self.composer.attachments.is_empty() {
            ComposerInput::Chat("")
        } else {
            self.composer.parse_submission(&raw)
        };
        if !matches!(parsed, ComposerInput::Chat(_)) && !self.thread_writable() {
            self.push_notice("Read-only attach: explicitly Start or Claim before sending.");
            return;
        }
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
                        let (prompt, withheld_notice) = match self.expand_skill_mentions(text) {
                            Ok(expansion) => expansion,
                            Err(notice) => {
                                self.push_notice(notice);
                                return;
                            }
                        };
                        let submission = ChatSubmission {
                            submission_id: uuid::Uuid::new_v4().to_string(),
                            fork_seed: self.fork_seed(thread_id),
                            project_root: self
                                .thread_project(thread_id)
                                .map(|project| project.repo_root.clone()),
                            composer_role: self.composer.role,
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
                            text: prompt.clone(),
                            model_preference: self
                                .sidebar
                                .threads
                                .iter()
                                .find(|thread| &thread.id == thread_id)
                                .and_then(|thread| thread.model_preference.as_ref())
                                .map(|preference| runtime::ModelPreference {
                                    profile: preference.profile.clone(),
                                    model: preference.model.clone(),
                                    reasoning_effort: preference.reasoning_effort.clone(),
                                }),
                        };
                        let permit = match self.chat_permit(&submission.thread_id) {
                            Ok(permit) => permit,
                            Err(notice) => {
                                self.push_notice(notice);
                                return;
                            }
                        };
                        let title_chat =
                            self.title_candidate(&submission).then(|| ChatSubmission {
                                text: text.into(),
                                ..submission.clone()
                            });
                        let previous_history = self.history.clone();
                        let previous_drafts = self.composer_drafts.clone();
                        let previous_sidebar = self.sidebar.clone();
                        let resent_from = self
                            .composer_drafts
                            .iter()
                            .find(|draft| draft.thread_id == submission.thread_id)
                            .map(|draft| draft.origin.clone());
                        let origins: Vec<_> = self
                            .composer_drafts
                            .iter()
                            .filter(|draft| draft.thread_id == submission.thread_id)
                            .map(|draft| draft.origin.clone())
                            .collect();
                        for message in &mut self.history {
                            if origins.contains(&message.id) {
                                message.resolution = Some(super::history::Resolution::Resent {
                                    new_id: submission.submission_id.clone(),
                                });
                            }
                        }
                        self.composer_drafts
                            .retain(|draft| draft.thread_id != submission.thread_id);
                        let sent_at = std::time::SystemTime::now();
                        if let Some(thread) = self
                            .sidebar
                            .threads
                            .iter_mut()
                            .find(|thread| thread.id.to_string() == submission.thread_id)
                        {
                            thread
                                .chat_role
                                .get_or_insert(submission.composer_role.into());
                            thread.touch(sent_at);
                            self.composer.restore_thread_role(thread);
                        }
                        // A busy run reads follow-ups later; they wait above the composer
                        // and join the transcript once delivered. A message that starts
                        // a run has no inbox to wait in, so it shows as sent at once and
                        // receipts only take it back if it never arrives.
                        let durable = self.sink.has_host_input_receipts();
                        let queued = self
                            .follow_up_would_queue(&submission.thread_id)
                            .then(|| submission.thread_id.clone());
                        let starts_run = durable && queued.is_none();
                        self.history.push(super::history::UserMessage {
                            id: submission.submission_id.clone(),
                            thread_id: submission.thread_id.clone(),
                            text: text.into(),
                            prompt: prompt.clone(),
                            at: sent_at,
                            image_count: submission.images.len(),
                            resent_from,
                            status: DeliveryStatus::Queued,
                            starts_run,
                            resolution: None,
                        });
                        if let Some(thread) = self
                            .sidebar
                            .threads
                            .iter_mut()
                            .find(|thread| thread.id.to_string() == submission.thread_id)
                        {
                            thread.draft_input.clear();
                        }
                        if let Err(error) = self.persist_history() {
                            self.history = previous_history;
                            self.composer_drafts = previous_drafts;
                            self.sidebar = previous_sidebar;
                            self.push_notice(format!("Could not save before sending: {error}"));
                            return;
                        }
                        let submission_id = submission.submission_id.clone();
                        self.queued_images
                            .insert(submission_id.clone(), self.composer.attachments.clone());
                        let submission_thread = submission.thread_id.clone();
                        self.issued
                            .push(WorkbenchCommand::SendChat(submission.clone()));
                        let events = match permit {
                            Some(permit) => self.sink.submit_chat_with_permit(submission, permit),
                            None => self.sink.submit(WorkbenchCommand::SendChat(submission)),
                        };
                        let rejection = events.iter().find_map(|event| match event {
                            LoopEvent::ChatRejected { reason, .. }
                            | LoopEvent::CommandRejected { reason } => Some(reason.clone()),
                            _ => None,
                        });
                        match rejection {
                            // The agent never saw it: no bubble, and the composer keeps
                            // the text and attachments to edit and send again.
                            Some(reason) => {
                                self.reject_submission(&submission_id, reason);
                            }
                            None => {
                                if let Some(chat) = title_chat {
                                    self.start_auto_title(chat);
                                }
                                match queued {
                                    Some(thread) => {
                                        self.queue_follow_up(
                                            &thread,
                                            submission_id,
                                            text.into(),
                                            prompt,
                                        );
                                    }
                                    None if durable => {
                                        // Receipts decide the outcome. Keyed by the
                                        // submission, so Delivered cannot add a second bubble.
                                        self.transcripts.push_host_input(
                                            &submission_thread,
                                            &submission_id,
                                            prompt,
                                        );
                                        self.save_sidebar();
                                    }
                                    None => {
                                        // The runtime echoes the expanded prompt; matching it
                                        // avoids a duplicate bubble.
                                        self.transcripts.push_thread(
                                            TranscriptEntry::UserMessage { text: prompt },
                                        );
                                        self.mark_delivered(&[submission_id]);
                                        self.save_sidebar();
                                    }
                                }
                                // Only an accepted message says its repository skills went
                                // out as plain text; a refused one goes back to the composer.
                                if let Some(notice) = withheld_notice {
                                    self.push_notice(notice);
                                }
                                self.composer.input.clear();
                                self.composer.attachments.clear();
                            }
                        }
                        for event in events {
                            self.apply_loop_event(event);
                        }
                    }
                }
            }
            ComposerInput::Command { spec, args } => match spec.name {
                "compaction" => {
                    if !args.is_empty() {
                        self.push_notice("usage: /compaction");
                        return;
                    }
                    let Some(thread_id) = self.sidebar.active_thread.as_ref() else {
                        self.push_notice("Select or start a thread first");
                        return;
                    };
                    self.submit_command(WorkbenchCommand::CompactChat {
                        thread_id: thread_id.to_string(),
                    });
                    self.composer.input.clear();
                }
                "continue" => {
                    if !args.is_empty() {
                        self.push_notice("usage: /continue");
                        return;
                    }
                    let (Some(_), Some(thread_id)) = (
                        self.sidebar.selected_project.as_ref(),
                        self.sidebar.active_thread.as_ref(),
                    ) else {
                        self.push_notice("Select or start a thread first");
                        return;
                    };
                    if let ProviderStatus::NotConfigured { guidance } = &self.provider_status {
                        self.push_notice(guidance.clone());
                        return;
                    }
                    let command =
                        WorkbenchCommand::ContinueChat(self.chat_continuation(thread_id, false));
                    self.submit_command(command);
                    self.composer.input.clear();
                }
                "run" => {
                    if args.is_empty() {
                        self.push_notice("usage: /run <text>");
                        return;
                    }
                    match &self.provider_status {
                        ProviderStatus::NotConfigured { guidance } => {
                            self.push_notice(guidance.clone());
                        }
                        ProviderStatus::Configured => {
                            match self.sink.start_background_run(args.into()) {
                                Some(run_id) => {
                                    self.push_notice(format!("Started background run {run_id}"));
                                    self.composer.input.clear();
                                }
                                None => self.push_notice("Background runs are unavailable"),
                            }
                        }
                    }
                }
                "team" => {
                    if let Some((value, goal)) = args.split_once('|')
                        && !value.trim().is_empty()
                        && !goal.trim().is_empty()
                    {
                        self.goal_form.delegation_value = Some(value.trim().into());
                        self.goal_form.goal = goal.trim().into();
                        self.submit_goal();
                        self.goal_form.delegation_value = None;
                        self.composer.input.clear();
                    } else {
                        self.push_notice("usage: /team <delegation value> | <goal>");
                    }
                }
                "goal" => {
                    if args.is_empty() {
                        self.push_notice("usage: /goal <text>");
                    } else {
                        self.goal_form.goal = args.into();
                        self.goal_form.delegation_value = None;
                        self.submit_goal();
                        self.composer.input.clear();
                    }
                }
                "help" => {
                    self.push_notice(self.composer.registry.help_text());
                    self.composer.input.clear();
                }
                "new" => {
                    let command = std::mem::take(&mut self.composer.input);
                    if let Err(error) = self.create_thread("New thread") {
                        self.composer.input = command;
                        self.push_notice(format!("failed to create thread: {error}"));
                    }
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
                        Some(root) => {
                            if self.external_command_running() {
                                self.push_notice("An external command is already running");
                                return;
                            }
                            let registry = self.composer.registry.clone();
                            self.start_external(async move {
                                registry
                                    .execute(&raw, &root)
                                    .await
                                    .map(super::external_commands::Outcome::Output)
                            });
                            self.composer.input.clear();
                        }
                        None => self.push_notice("Select a project first"),
                    }
                } else {
                    self.push_notice(format!("unknown command /{name} — type /help"));
                }
            }
        }
        self.flush_composer_draft();
    }

    /// Inlines skills written as `@name`, read fresh so edits apply on the next send.
    ///
    /// Like the runtime, an untrusted project's own skills stay out: their
    /// mentions are sent as plain text, and the notice returned beside the
    /// prompt names them and says how to let them in.
    fn expand_skill_mentions(&self, text: &str) -> Result<(String, Option<String>), String> {
        if !text.contains('@') {
            return Ok((text.into(), None));
        }
        let project = self.active_project();
        let root = project.map(|project| project.repo_root.as_path());
        let repo_skills = repo_skills_admitted(project);
        let skills = mention_skills(root, repo_skills);
        let prompt = crate::model::composer::expand_skill_mentions(text, root, &skills)
            .map_err(|name| format!("Could not load skill @{name}"))?;
        let notice = root.filter(|_| !repo_skills).and_then(|root| {
            let trusted = mention_skills(Some(root), true);
            let withheld = withheld_skill_mentions(text, Some(root), &skills, &trusted);
            (!withheld.is_empty()).then(|| untrusted_skills_notice(&withheld))
        });
        Ok((prompt, notice))
    }

    pub(super) fn push_notice(&mut self, text: impl Into<String>) {
        self.transcripts
            .push_thread(TranscriptEntry::Notice { text: text.into() });
    }

    /// A notice for a rejected command: the reason in English, the original in the hover.
    pub(super) fn push_reason_notice(&mut self, prefix: &str, reason: &str) {
        self.transcripts
            .push_thread(crate::model::transcript::reason_entry(
                prefix, reason, false,
            ));
    }

    pub(super) fn refresh_image_capability(&mut self) {
        let Some(model) = self.active_model() else {
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
                runtime::AgentModel::selected_model(model.as_ref(), runtime::Role::Worker, None)
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
