//! The composer at the bottom of the conversation, laid out and keyed as the embedded
//! workbench's: slash command and `@` file completions above the role and team
//! selectors, the attached files as chips, and the input with the attach "+" on its
//! left and Stop, Queue and Send on its right.
use egui::Ui;
use workbench_protocol::namespaces::{ConversationCommand, ExecutionCommand};
use workbench_protocol::{commands::*, ids::*};

use super::skills::{self, SkillSend};
use super::slash::{self, Ctx, OFFERED, Outcome};
use super::{Activity, ConversationIo, ConversationPane, stop};
use crate::attachments::Intake;
use crate::composer_input::{CompletionItem, mention_query};
use crate::core_client::connection::Action;
use crate::core_client::features::Navigation;
use crate::core_client::shell::Effect;
use crate::panes::attachment_widgets::{
    AttachmentRowStatus, AttachmentRowView, attach_plus_button, attachment_chips, preparing,
    retry_upload_button,
};
use crate::panes::completion_widgets::{completion_keys, completion_list};
use crate::panes::transcript_widgets::{ime_composing, stop_button as shared_stop_button};
use crate::theme::icons;
use crate::theme::tokens::*;
use crate::theme::widgets::{ghost, primary_icon_button, surface_frame};

const MAX_NOTICES: usize = 20;
const WAITING_FOR_SKILLS: &str =
    "Reading the project's skills for the @mentions. Send again in a moment.";
const TOO_MANY_SKILLS: &str = "A message can name at most 32 skills.";
const SKILLS_UNLISTED: &str =
    "The skill list could not be read, so the @mentions were sent as plain text.";

impl ConversationPane {
    /// Before egui reads the input of a frame, while the composer has focus: Tab and
    /// Shift+Tab go to the completions instead of moving focus, and an empty paste
    /// (the platform's image paste) or dropped files become attachments of the
    /// selected thread. An empty paste is never text, so it is always dropped.
    pub fn capture_input(
        &mut self,
        raw: &mut egui::RawInput,
        project: Option<&ProjectId>,
        thread: Option<&ThreadId>,
    ) {
        let paste = |raw: &egui::RawInput, empty: bool| {
            raw.events
                .iter()
                .any(|event| matches!(event, egui::Event::Paste(text) if text.is_empty() == empty))
        };
        let image_paste = paste(raw, true) && !paste(raw, false);
        raw.events
            .retain(|event| !matches!(event, egui::Event::Paste(text) if text.is_empty()));
        if !self.completion.focused {
            return;
        }
        if let (Some(project), Some(thread)) = (project, thread) {
            if image_paste {
                self.intakes
                    .push((project.clone(), thread.clone(), Intake::Clipboard));
            }
            if !raw.dropped_files.is_empty() {
                let files = std::mem::take(&mut raw.dropped_files)
                    .into_iter()
                    .map(|file| file.path().to_path_buf())
                    .collect();
                self.intakes
                    .push((project.clone(), thread.clone(), Intake::Files(files)));
            }
        }
        let tabs = &mut self.completion.tab_presses;
        raw.events.retain(|event| match event {
            egui::Event::Key {
                key: egui::Key::Tab,
                pressed,
                repeat,
                modifiers,
                ..
            } if modifiers.is_none() || modifiers.shift_only() => {
                if *pressed && !repeat {
                    tabs.push(modifiers.shift);
                }
                false
            }
            egui::Event::Text(text) | egui::Event::Ime(egui::ImeEvent::Commit(text))
                if text == "\t" =>
            {
                false
            }
            _ => true,
        });
    }

    pub fn set_mention_scope(&mut self, roots: &crate::core_client::local::LocalRoots) {
        self.mentions.set_scope(roots);
    }

    /// Blocks until an `@` index build in flight has landed.
    #[cfg(test)]
    pub(crate) fn wait_mentions(&mut self) {
        self.mentions.wait();
    }

    /// Puts the cursor in the composer the next time it is drawn.
    pub(in crate::core_client) fn request_composer_focus(&mut self) {
        self.completion.focus_requested = true;
    }

    /// A selection change a slash command asked for, for the dock pass to apply.
    pub fn take_navigation(&mut self) -> Option<Navigation> {
        self.navigation.take()
    }

    /// Before the dock draws: only a composer drawn in this frame claims Tab, drops and
    /// pastes in the next hook, as the embedded workbench clears its focus before it
    /// renders. A hidden one must not keep claiming them.
    pub fn begin_frame(&mut self) {
        self.was_focused = std::mem::take(&mut self.completion.focused);
    }

    /// After the dock pass: Tab presses only apply to the frame they were captured for.
    pub fn end_frame(&mut self) {
        self.completion.tab_presses.clear();
    }

    /// Files arriving while the thread's message is being sent are refused: the core's
    /// reply clears the thread's pending files, so they would be lost.
    pub(super) fn submit_intakes(
        &mut self,
        ctx: &egui::Context,
        sending: &std::collections::BTreeSet<String>,
        effects: &mut Vec<Effect>,
    ) {
        for (project, thread, intake) in std::mem::take(&mut self.intakes) {
            if sending.contains(thread.as_str()) {
                effects.push(Effect::Status(
                    crate::core_client::WAIT_FOR_SEND_TO_ATTACH.to_owned(),
                ));
                continue;
            }
            match self.attachment_io.submit(thread.as_str(), intake, ctx) {
                Ok(request) => effects.push(Effect::AttachRequested {
                    request,
                    project,
                    thread,
                }),
                Err(error) => effects.push(Effect::Status(error)),
            }
        }
    }

    fn notice(&mut self, thread: &ThreadId, text: String) {
        let notices = self.notices.entry(thread.to_string()).or_default();
        notices.push(text);
        if notices.len() > MAX_NOTICES {
            notices.remove(0);
        }
    }
}

pub(super) fn composer(
    ui: &mut Ui,
    pane: &mut ConversationPane,
    io: &mut ConversationIo<'_>,
    project: &ProjectId,
    thread: &ThreadId,
    activity: Activity,
    effects: &mut Vec<Effect>,
) {
    if pane.completion_thread.as_ref() != Some(thread) {
        pane.completion_thread = Some(thread.clone());
        pane.completion.dismissed_for = None;
        pane.completion.selected = 0;
    }
    // One id for the input wherever the composer is drawn (a new conversation's screen and
    // a thread's differ in their ui hierarchy), so the focus and the cursor survive the
    // new conversation becoming the thread.
    let input_id = egui::Id::new("core-client-composer-input");
    let min_input = COMPOSER_MIN_HEIGHT - 2.0 * SP_2;
    // Wrapped rows are only known after layout, so the viewport follows the
    // last measured document height and the pass is redone when it changes.
    let measured_id = input_id.with("wanted-height");
    let wanted = ui
        .data(|data| data.get_temp::<f32>(measured_id))
        .unwrap_or_else(|| {
            let text = io
                .drafts
                .get(thread.as_str())
                .map_or("", |d| d.text.as_str());
            let rows = text.split('\n').count().max(1);
            rows as f32 * ui.text_style_height(&egui::TextStyle::Body) + 2.0 * SP_2
        })
        .clamp(min_input, COMPOSER_MAX_HEIGHT);
    let mut measured = wanted;
    let frame = surface_frame(palette().SURFACE_RAISED)
        .corner_radius(R_2XL)
        .inner_margin(egui::vec2(SP_3, SP_2))
        .show(ui, |ui| {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(SP_2, SP_1);
                let ime_id = input_id.with("ime-composing");
                let ime = ime_composing(ui, ime_id);
                // Escape clears egui's focus before the frame, so it is matched
                // against the focus the editor held at the end of the last one.
                let was_focused = pane.was_focused;
                let focused = ui.memory(|memory| memory.has_focus(input_id));
                let items = completions(ui, pane, io, project, thread, ime, focused, effects);
                // As in the embedded workbench, open completions hide the working
                // root's Stop and Queue; Stop subagents stays.
                let completing = !items.is_empty();
                let can_stop = (activity.root_active && !completing) || activity.stopped;
                selectors(ui, pane);
                let can_choose = !pane.model_saving && !io.sending.contains(thread.as_str()) && io.connected;
                if let Some(choice) = pane.model_picker.show(ui, pane.model_catalog.as_ref(), &pane.model_choice, pane.composer_role == ComposerRole::Orchestrator, can_choose) {
                    if io.cache.threads.contains_key(thread.as_str()) {
                        pane.model_saving = true;
                        effects.push(Effect::ModelChoice(thread.clone(), choice));
                    } else {
                        pane.draft_model_choices.insert(thread.to_string(), choice.clone());
                        pane.model_choice = choice;
                    }
                }
                attachments(ui, io, project, thread, effects);
                // As tall as the input, so the attach "+" on its left and the buttons on
                // its right sit at its bottom.
                let row = egui::vec2(ui.available_width(), wanted);
                let bottom_up = egui::Layout::left_to_right(egui::Align::BOTTOM);
                ui.allocate_ui_with_layout(row, bottom_up, |ui| {
                    attach_button(ui, pane, io.sending, project, thread, effects);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::BOTTOM), |ui| {
                        let draft_text = io
                            .drafts
                            .get(thread.as_str())
                            .map_or("", |d| d.text.as_str());
                        let has_attachments = io
                            .attachment_drafts
                            .get(thread.as_str())
                            .is_some_and(|files| !files.is_empty());
                        let can_send = !pane.model_saving && io.can_send(thread.as_str())
                            && (!draft_text.trim().is_empty() || has_attachments);
                        let mut send = false;
                        if can_stop
                            && shared_stop_button(ui, activity.stopped, activity.running_children)
                                .clicked()
                        {
                            effects.push(Effect::send(stop(thread)));
                        }
                        let send_button = if activity.root_active && !completing {
                            ui.add_enabled(can_send, egui::Button::new("Queue"))
                                .on_hover_text("Queue as a follow-up (Enter)")
                        } else if can_send {
                            primary_icon_button(ui, icons::PAPER_PLANE_RIGHT, "Send")
                        } else {
                            ui.add_enabled(
                                false,
                                ghost(icons::with_icon(icons::PAPER_PLANE_RIGHT, "Send")),
                            )
                        };
                        send |= send_button.clicked();
                        let draft = io.drafts.entry(thread.to_string()).or_default();
                        let input = egui::ScrollArea::vertical()
                            .id_salt("composer-scroll")
                            .min_scrolled_width(0.0)
                            .min_scrolled_height(wanted)
                            .max_height(wanted)
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                ui.with_layout(egui::Layout::top_down(egui::Align::LEFT), |ui| {
                                    if let Some(cursor) = pane.completion.pending_cursor.take() {
                                        let mut state =
                                            egui::text_edit::TextEditState::load(ui.ctx(), input_id)
                                                .unwrap_or_default();
                                        state.cursor.set_char_range(Some(
                                            egui::text::CCursorRange::one(egui::text::CCursor::new(
                                                cursor,
                                            )),
                                        ));
                                        state.store(ui.ctx(), input_id);
                                    }
                                    ui.add(
                                        egui::TextEdit::multiline(&mut draft.text)
                                            .id(input_id)
                                            .hint_text(
                                                "Message, /command or @file  (Shift+Enter: newline)",
                                            )
                                            // Tab is captured for the completions before
                                            // egui sees it; a leaked one must not move focus.
                                            .lock_focus(true)
                                            .desired_rows(1)
                                            .desired_width(f32::INFINITY)
                                            .min_size(egui::vec2(
                                                0.0,
                                                COMPOSER_MIN_HEIGHT - 2.0 * SP_2,
                                            ))
                                            .return_key(egui::KeyboardShortcut::new(
                                                egui::Modifiers::SHIFT,
                                                egui::Key::Enter,
                                            ))
                                            .frame(
                                                egui::Frame::NONE
                                                    .inner_margin(egui::vec2(SP_3, SP_2)),
                                            )
                                            .background_color(palette().INPUT),
                                    )
                                })
                                .inner
                            })
                            .inner;
                        measured = input.rect.height().clamp(min_input, COMPOSER_MAX_HEIGHT);
                        input.widget_info(|| {
                            egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, "Message")
                        });
                        if input.changed() {
                            draft.revision += 1;
                        }
                        let enter = input.has_focus()
                            && !ime
                            && ui.input(|i| {
                                i.key_pressed(egui::Key::Enter)
                                    && !i.modifiers.shift
                                    && !i.modifiers.command
                            });
                        if std::mem::take(&mut pane.completion.focus_requested) {
                            input.request_focus();
                        }
                        pane.completion.focused = input.has_focus();
                        pane.completion.cursor =
                            egui::text_edit::TextEditState::load(ui.ctx(), input_id)
                                .and_then(|state| state.cursor.char_range())
                                .map(|range| range.primary.index.0);
                        ui.data_mut(|d| d.insert_temp(ime_id, ime && pane.completion.focused));
                        if was_focused && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                            if !items.is_empty() {
                                pane.completion.dismiss(&draft.text);
                            } else if can_stop {
                                effects.push(Effect::send(stop(thread)));
                            }
                            input.request_focus();
                        } else if (send || enter) && can_send {
                            submit(pane, io, project, thread, effects);
                            input.request_focus();
                        }
                    });
                });
            });
        });
    if (measured - wanted).abs() > 0.5 {
        ui.data_mut(|data| data.insert_temp(measured_id, measured));
        ui.ctx().request_discard("composer input rows changed");
        ui.ctx().request_repaint();
    }
    // The panel below the transcript is sized from the last pass, so a list of
    // completions or files that appears or goes is laid out again at once instead of
    // spilling under the panels beneath it for a frame.
    let height_id = input_id.with("frame-height");
    let height = frame.response.rect.height();
    let last = ui.data(|data| data.get_temp::<f32>(height_id));
    if last.is_none_or(|last| (last - height).abs() > 0.5) {
        ui.data_mut(|data| data.insert_temp(height_id, height));
        if last.is_some() {
            ui.ctx().request_discard("composer height changed");
            ui.ctx().request_repaint();
        }
    }
}

/// The completion keys and list of the draft; an accepted candidate is applied to
/// the draft at once. Returns the candidates shown.
fn completions(
    ui: &mut Ui,
    pane: &mut ConversationPane,
    io: &mut ConversationIo<'_>,
    project: &ProjectId,
    thread: &ThreadId,
    ime: bool,
    focused: bool,
    effects: &mut Vec<Effect>,
) -> Vec<CompletionItem> {
    let text = io
        .drafts
        .get(thread.as_str())
        .map_or("", |draft| draft.text.as_str());
    let mention = mention_query(text, pane.completion.cursor).is_some();
    if pane.mentions.refresh(ui.ctx(), mention) {
        // The skills offered are read again each time a mention starts, so a skill
        // added meanwhile completes without a watcher.
        effects.push(Effect::RefreshSkills(project.clone()));
    }
    let items = pane.completion.items(text, OFFERED, &pane.mentions.index);
    let state = &mut pane.completion;
    state.selected = state.selected.min(items.len().saturating_sub(1));
    let tabs = std::mem::take(&mut state.tab_presses);
    let keyed = completion_keys(ui, &items, text, &mut state.selected, tabs, ime, focused);
    let picked = completion_list(ui, &items, state.selected).or(keyed);
    if let Some(item) = picked {
        let draft = io.drafts.entry(thread.to_string()).or_default();
        if state.accept(&mut draft.text, &item) {
            draft.revision += 1;
        }
    }
    items
}

fn selectors(ui: &mut Ui, pane: &mut ConversationPane) {
    ui.horizontal_wrapped(|ui| {
        let (icon, label) = match pane.composer_role {
            ComposerRole::Worker => (icons::ROBOT, "Worker"),
            ComposerRole::Orchestrator => (icons::TREE_STRUCTURE, "Orchestrator"),
        };
        let response = ui
            .button(icons::with_icon(icon, label))
            .on_hover_text("Switch the role that receives your message");
        let accessible = format!("Role: {label}");
        response
            .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &accessible));
        if response.clicked() {
            pane.composer_role = match pane.composer_role {
                ComposerRole::Worker => ComposerRole::Orchestrator,
                ComposerRole::Orchestrator => ComposerRole::Worker,
            };
        }
        ui.toggle_value(
            &mut pane.team,
            icons::with_icon(icons::USERS_THREE, "Team objective"),
        )
        .on_hover_text(
            "Send the message as an objective for a team. Attachments are not supported.",
        );
    });
}

/// The "+" left of the input opens the file picker; it waits while the thread's
/// message is being sent, as pasted and dropped files do.
fn attach_button(
    ui: &mut Ui,
    pane: &mut ConversationPane,
    sending: &std::collections::BTreeSet<String>,
    project: &ProjectId,
    thread: &ThreadId,
    effects: &mut Vec<Effect>,
) {
    let enabled = !sending.contains(thread.as_str());
    let picked = ui
        .add_enabled_ui(enabled, |ui| {
            attach_plus_button(ui)
                .on_disabled_hover_text(crate::core_client::WAIT_FOR_SEND_TO_ATTACH)
                .clicked()
        })
        .inner;
    if picked {
        pane.intakes
            .push((project.clone(), thread.clone(), Intake::ChooseFiles));
        pane.submit_intakes(ui.ctx(), sending, effects);
    }
}

/// The files being read and a chip per file with Remove, in one wrapping row; Retry
/// upload offers again what the core did not take.
fn attachments(
    ui: &mut Ui,
    io: &ConversationIo<'_>,
    project: &ProjectId,
    thread: &ThreadId,
    effects: &mut Vec<Effect>,
) {
    let sending = io.sending.contains(thread.as_str());
    let loading = io
        .loading_attachments
        .get(thread.as_str())
        .copied()
        .unwrap_or(0);
    let files = io
        .attachment_drafts
        .get(thread.as_str())
        .map(Vec::as_slice)
        .unwrap_or_default();
    if loading == 0 && files.is_empty() {
        return;
    }
    let rows: Vec<_> = files
        .iter()
        .map(|file| AttachmentRowView {
            key: &file.id,
            filename: &file.file.filename,
            media_type: &file.file.media_type,
            bytes: file.file.bytes.len(),
            status: if file.reference.is_some() {
                AttachmentRowStatus::Ready
            } else if file.uploading {
                AttachmentRowStatus::Uploading
            } else if file.uncertain {
                AttachmentRowStatus::Failed("Upload state unknown")
            } else {
                AttachmentRowStatus::Failed("Upload failed")
            },
            removable: !sending && !file.uploading,
        })
        .collect();
    let pending = files
        .iter()
        .any(|file| file.reference.is_none() && !file.uploading && !file.uncertain);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = egui::vec2(SP_1, SP_1);
        preparing(ui, loading);
        if let Some(index) = attachment_chips(ui, &rows) {
            effects.push(Effect::RemoveAttachment {
                thread: thread.clone(),
                id: files[index].id.clone(),
            });
        }
        if pending && retry_upload_button(ui).clicked() {
            effects.push(Effect::RetryAttachments {
                project: project.clone(),
                thread: thread.clone(),
            });
        }
    });
}

/// Carries out a slash command, or sends the draft as a message (or, with Team
/// objective on, as a team's objective).
fn submit(
    pane: &mut ConversationPane,
    io: &mut ConversationIo<'_>,
    project: &ProjectId,
    thread: &ThreadId,
    effects: &mut Vec<Effect>,
) {
    if pane.model_saving {
        return;
    }
    let exists = io.cache.threads.contains_key(thread.as_str());
    let Some(draft) = io.drafts.get_mut(thread.as_str()) else {
        return;
    };
    let ctx = Ctx {
        project,
        thread,
        exists,
        role: pane.composer_role,
    };
    let clear = |draft: &mut crate::core_client::Draft| {
        draft.text.clear();
        draft.revision += 1;
    };
    match slash::dispatch(&draft.text, &ctx) {
        Outcome::Chat => {}
        // As a message, the text stays until the core accepts it, so a refused
        // command keeps it; the files wait for the next message (see `takes_files`).
        Outcome::Command(command) => {
            effects.push(Effect::send(Action::Command {
                command: *command,
                draft: Some((thread.to_string(), draft.revision)),
            }));
            pane.notices.remove(thread.as_str());
            return;
        }
        Outcome::Help(text) => {
            clear(draft);
            pane.notice(thread, text);
            return;
        }
        Outcome::Usage(text) => {
            pane.notice(thread, text);
            return;
        }
        Outcome::NewConversation => {
            clear(draft);
            pane.navigation = Some(Navigation::NewConversation(project.clone()));
            return;
        }
    }
    let attachments: Vec<_> = io
        .attachment_drafts
        .get(thread.as_str())
        .into_iter()
        .flatten()
        .filter_map(|i| i.reference.as_ref().map(|reference| reference.id.clone()))
        .collect();
    if pane.team && !attachments.is_empty() {
        effects.push(Effect::Status(
            "A team objective cannot carry attachments".into(),
        ));
        return;
    }
    let mut unlisted = false;
    let skills = if pane.team {
        Vec::new()
    } else {
        let known = pane.skills.project.as_ref() == Some(project);
        let state = pane.skills.state.as_ref().filter(|_| known);
        let fetching = known && pane.skills.fetching;
        match skills::resolve(&draft.text, state, fetching) {
            SkillSend::Send(names) => names,
            SkillSend::Unlisted => {
                unlisted = true;
                Vec::new()
            }
            SkillSend::Wait => {
                effects.push(Effect::RefreshSkills(project.clone()));
                pane.notice(thread, WAITING_FOR_SKILLS.into());
                return;
            }
            SkillSend::TooMany => {
                pane.notice(thread, TOO_MANY_SKILLS.into());
                return;
            }
        }
    };
    let command = if pane.team {
        let goal = SubmitGoal {
            project: project.clone(),
            thread: thread.clone(),
            objective: draft.text.clone(),
            references: vec![],
            constraints: vec![],
            delegation: Some(Delegation::Team {
                value: draft.text.clone(),
            }),
        };
        if exists {
            Command::Execution(ExecutionCommand::SubmitGoal(goal))
        } else {
            Command::Conversation(ConversationCommand::CreateGoal(goal))
        }
    } else {
        let chat = SendChat {
            project: project.clone(),
            thread: thread.clone(),
            submission_id: SubmissionId::new(uuid::Uuid::new_v4().to_string()).unwrap(),
            composer_role: pane.composer_role,
            text: draft.text.clone(),
            skills,
            attachments,
            model_preference: if exists {
                None
            } else {
                match &pane.model_choice {
                    ModelChoice::RoleDefault => None,
                    ModelChoice::Explicit(p) => Some(p.clone()),
                }
            },
            fork_seed: None,
        };
        if exists {
            Command::Conversation(ConversationCommand::SendChat(chat))
        } else {
            Command::Conversation(ConversationCommand::CreateExecution(chat))
        }
    };
    effects.push(Effect::send(Action::Command {
        command,
        draft: Some((thread.to_string(), draft.revision)),
    }));
    pane.notices.remove(thread.as_str());
    if unlisted {
        pane.notice(thread, SKILLS_UNLISTED.into());
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use egui::{Event, Key, Modifiers, RawInput};

    use super::*;

    #[derive(Debug)]
    struct Dropped(PathBuf);

    impl egui::DroppedFile for Dropped {
        fn path(&self) -> &Path {
            &self.0
        }

        fn bytes(&self) -> Result<Vec<u8>, String> {
            Ok(Vec::new())
        }
    }

    fn tab(shift: bool) -> Event {
        Event::Key {
            key: Key::Tab,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: if shift {
                Modifiers::SHIFT
            } else {
                Modifiers::NONE
            },
        }
    }

    fn ids() -> (ProjectId, ThreadId) {
        (ProjectId::new("p").unwrap(), ThreadId::new("t").unwrap())
    }

    fn capture(pane: &mut ConversationPane, events: Vec<Event>) -> RawInput {
        let (project, thread) = ids();
        let mut raw = RawInput {
            events,
            ..RawInput::default()
        };
        pane.capture_input(&mut raw, Some(&project), Some(&thread));
        raw
    }

    fn intakes(pane: &ConversationPane) -> Vec<String> {
        pane.intakes
            .iter()
            .map(|(_, thread, intake)| match intake {
                Intake::Clipboard => format!("{thread}: clipboard"),
                Intake::Files(paths) => format!("{thread}: {paths:?}"),
                Intake::ChooseFiles => format!("{thread}: choose"),
            })
            .collect()
    }

    // Given: the composer focused
    // When: Tab, Shift+Tab, a typed tab and a key repeat arrive
    // Then: the presses are kept for the completions and none reaches egui
    #[test]
    fn a_focused_composer_takes_tab_for_its_completions() {
        let mut pane = ConversationPane::default();
        pane.completion.focused = true;
        let mut repeat = tab(false);
        if let Event::Key { repeat: r, .. } = &mut repeat {
            *r = true;
        }
        let a = Event::Text("a".into());
        let raw = capture(
            &mut pane,
            vec![
                tab(false),
                tab(true),
                Event::Text("\t".into()),
                repeat,
                a.clone(),
            ],
        );
        assert_eq!(raw.events, [a]);
        assert_eq!(pane.completion.tab_presses, [false, true]);
    }

    // Given: the composer not focused
    // Then: Tab and dropped files are left alone, and an empty paste still goes
    #[test]
    fn an_unfocused_composer_leaves_the_input_alone() {
        let mut pane = ConversationPane::default();
        let (project, thread) = ids();
        let mut raw = RawInput {
            events: vec![tab(false), Event::Paste(String::new())],
            dropped_files: vec![Arc::new(Dropped("/tmp/a.png".into()))],
            ..RawInput::default()
        };
        pane.capture_input(&mut raw, Some(&project), Some(&thread));
        assert_eq!(raw.events, [tab(false)]);
        assert_eq!(raw.dropped_files.len(), 1);
        assert!(pane.completion.tab_presses.is_empty() && pane.intakes.is_empty());
    }

    // Given: the composer focused on a selected thread
    // When: an empty paste alone, an empty paste beside text, and dropped files arrive
    // Then: the lone empty paste reads the clipboard, the text paste stays text, and
    //       the dropped files are attached
    #[test]
    fn an_image_paste_and_dropped_files_become_attachments() {
        let mut pane = ConversationPane::default();
        pane.completion.focused = true;
        let raw = capture(&mut pane, vec![Event::Paste(String::new())]);
        assert!(raw.events.is_empty());
        assert_eq!(intakes(&pane), ["t: clipboard"]);

        pane.intakes.clear();
        let text = Event::Paste("hello".into());
        let raw = capture(&mut pane, vec![Event::Paste(String::new()), text.clone()]);
        assert_eq!(raw.events, [text]);
        assert!(pane.intakes.is_empty());

        let (project, thread) = ids();
        let mut raw = RawInput {
            dropped_files: vec![Arc::new(Dropped("/tmp/a.png".into()))],
            ..RawInput::default()
        };
        pane.capture_input(&mut raw, Some(&project), Some(&thread));
        assert!(raw.dropped_files.is_empty());
        assert_eq!(intakes(&pane), [r#"t: ["/tmp/a.png"]"#]);
    }
}
