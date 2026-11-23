//! The conversation tab of the core client, laid out as the embedded workbench's
//! conversation pane: a header, the committed transcript, the cards that wait for the
//! person, and the composer at the bottom. It draws only what the core has committed.
use std::collections::{BTreeMap, BTreeSet};

use egui::{RichText, Ui};
use workbench_protocol::namespaces::{
    ApprovalsCommand, ConversationCommand, ExecutionCommand, QuestionsCommand,
};
use workbench_protocol::{commands::*, ids::*, views::*};
use workspace_ui::ThreadRunPhase;

use super::cache::Cache;
use super::connection::Action;
use super::features::{FeatureId, Selection};
use super::shell::Effect;
use super::{ClientAttachmentDraft, Draft};
use crate::attachments::{AttachmentIo, Intake};
use crate::panes::markdown_render::render_markdown_with_base;
use crate::panes::phase_indicator::phase_circle;
use crate::panes::transcript_widgets::{
    code_block, diff_block, full_input, ime_composing, request_card, role_label, user_bubble,
};
use crate::theme::icons;
use crate::theme::text::{h3, muted};
use crate::theme::tokens::*;
use crate::theme::widgets::{
    empty_state, fill_label, ghost, icon_button, pane_root, primary_icon_button, soft_frame,
    surface_frame,
};

const MAX_ANSWER_BYTES: usize = 4096;

pub struct ConversationPane {
    answers: BTreeMap<String, String>,
    pub(super) attachment_io: AttachmentIo,
    pub(super) attachment_imports: BTreeMap<String, (ProjectId, ThreadId)>,
    composer_role: ComposerRole,
    team: bool,
    requested_bodies: BTreeSet<String>,
}

impl Default for ConversationPane {
    fn default() -> Self {
        Self {
            answers: BTreeMap::new(),
            attachment_io: AttachmentIo::default(),
            attachment_imports: BTreeMap::new(),
            composer_role: ComposerRole::Worker,
            team: false,
            requested_bodies: BTreeSet::new(),
        }
    }
}

pub struct ConversationIo<'a> {
    pub cache: &'a Cache,
    pub connected: bool,
    pub drafts: &'a mut BTreeMap<String, Draft>,
    pub attachment_drafts: &'a BTreeMap<String, Vec<ClientAttachmentDraft>>,
    pub sending: &'a BTreeSet<String>,
    pub loading_attachments: &'a BTreeMap<String, usize>,
}

impl ConversationIo<'_> {
    fn can_send(&self, thread: &str) -> bool {
        super::can_send(
            self.sending,
            self.loading_attachments,
            self.attachment_drafts,
            thread,
        )
    }

    fn can_discard_attachments(&self, thread: &str) -> bool {
        super::can_discard_attachments(
            self.sending,
            self.loading_attachments,
            self.attachment_drafts,
            thread,
        )
    }
}

pub fn show(
    ui: &mut Ui,
    pane: &mut ConversationPane,
    io: &mut ConversationIo<'_>,
    selection: &Selection,
    effects: &mut Vec<Effect>,
) {
    pane_root(ui, "Conversation", |ui| {
        let (Some(project), Some(thread)) = (selection.project.clone(), selection.thread.clone())
        else {
            empty_state(
                ui,
                "No conversation selected",
                "Pick a thread in Projects, or start a new conversation there.",
                None,
            );
            return;
        };
        if !io.connected {
            ui.colored_label(
                palette().WARNING_FG,
                "Not connected to the core. Nothing runs in this window in its place.",
            );
        }
        ui.add_enabled_ui(io.connected, |ui| {
            header(ui, io, &project, &thread, pane.composer_role, effects);
            egui::Panel::bottom(ui.id().with("composer"))
                .resizable(false)
                .show_separator_line(false)
                .frame(egui::Frame::NONE.inner_margin(egui::Margin::symmetric(0, SP_1 as i8)))
                .show(ui, |ui| {
                    queue_band(ui, io.cache, &thread, effects);
                    composer(ui, pane, io, &project, &thread, effects);
                    status_strip(ui, io.cache, &thread);
                });
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE)
                .show(ui, |ui| transcript(ui, pane, io, &thread, effects));
        });
    });
}

fn thread_phase(cache: &Cache, thread: &ThreadId) -> Option<ThreadRunPhase> {
    let state = cache.threads.get(thread.as_str())?.state;
    Some(match state {
        ThreadState::Idle => return None,
        ThreadState::Running => ThreadRunPhase::Running,
        ThreadState::WaitingForUser | ThreadState::WaitingForApproval => ThreadRunPhase::Waiting,
        ThreadState::Failed => ThreadRunPhase::Error,
    })
}

fn is_running(cache: &Cache, thread: &ThreadId) -> bool {
    cache.runs.values().any(|run| {
        &run.thread == thread
            && matches!(
                run.phase,
                RunPhase::Pending
                    | RunPhase::Running
                    | RunPhase::WaitingForUser
                    | RunPhase::WaitingForApproval
            )
    })
}

fn header(
    ui: &mut Ui,
    io: &ConversationIo<'_>,
    project: &ProjectId,
    thread: &ThreadId,
    role: ComposerRole,
    effects: &mut Vec<Effect>,
) {
    let title = io
        .cache
        .threads
        .get(thread.as_str())
        .map_or("New conversation", |t| t.title.as_str());
    egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(SP_1 as i8, SP_1 as i8))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.set_min_height(ROW_COMPACT - 2.0 * SP_2);
                phase_circle(ui, thread_phase(io.cache, thread));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.menu_button(icons::DOTS_THREE, |ui| {
                        if ui.button("Refresh history").clicked() {
                            effects.push(Effect::send(Action::History(thread.clone(), false)));
                            ui.close();
                        }
                        if ui.button("Deliver queued follow-ups next turn").clicked() {
                            effects.push(Effect::send(command(Command::Conversation(
                                ConversationCommand::DeliverFollowUpsNextTurn(ThreadTarget {
                                    thread: thread.clone(),
                                }),
                            ))));
                            ui.close();
                        }
                        if ui
                            .button("Take over stopped legacy thread")
                            .on_hover_text(
                                "Let the core continue a stopped thread the embedded workbench ran",
                            )
                            .clicked()
                        {
                            effects.push(Effect::send(command(Command::Conversation(
                                ConversationCommand::PrepareExistingThread {
                                    project: project.clone(),
                                    thread: thread.clone(),
                                },
                            ))));
                            ui.close();
                        }
                    })
                    .response
                    .on_hover_text("More");
                    if icon_button(ui, icons::BROWSER, "Preview")
                        .on_hover_text("Show the preview area")
                        .clicked()
                    {
                        effects.push(Effect::Reveal(FeatureId::Browser));
                    }
                    if icon_button(ui, icons::PACKAGE, "Artifacts")
                        .on_hover_text("Show what this thread produced")
                        .clicked()
                    {
                        effects.push(Effect::Reveal(FeatureId::Artifacts));
                    }
                    if is_running(io.cache, thread) {
                        if ui
                            .add(stop_button())
                            .on_hover_text("Stop the run. Its subagents keep running.")
                            .clicked()
                        {
                            effects.push(Effect::send(stop(thread)));
                        }
                    } else if io.cache.threads.contains_key(thread.as_str())
                        && ui
                            .add(ghost(icons::with_icon(icons::PLAY, "Continue")))
                            .on_hover_text("Resume the conversation without a new message")
                            .clicked()
                    {
                        effects.push(Effect::send(command(Command::Conversation(
                            ConversationCommand::ContinueChat(ContinueChat {
                                project: project.clone(),
                                thread: thread.clone(),
                                composer_role: role,
                                model_preference: None,
                            }),
                        ))));
                    }
                    let response = fill_label(ui, h3(title), ROW_DENSE, egui::Sense::hover());
                    let label = format!("Thread: {title}");
                    response.widget_info(|| {
                        egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &label)
                    });
                });
            });
        });
    ui.separator();
}

fn stop_button() -> egui::Button<'static> {
    egui::Button::new(RichText::new("Stop").color(palette().ERROR_FG)).fill(palette().ERROR_SURFACE)
}

fn stop(thread: &ThreadId) -> Action {
    command(Command::Execution(ExecutionCommand::StopChat(
        ThreadTarget {
            thread: thread.clone(),
        },
    )))
}

fn command(command: Command) -> Action {
    Action::Command {
        command,
        draft: None,
    }
}

enum Item<'a> {
    Message(&'a CommittedMessage),
    Tools(Vec<&'a CommittedMessage>),
}

fn items(history: &[CommittedMessage]) -> Vec<Item<'_>> {
    let mut items: Vec<Item<'_>> = Vec::new();
    for message in history {
        match (message.role, items.last_mut()) {
            (MessageRole::Tool, Some(Item::Tools(tools))) => tools.push(message),
            (MessageRole::Tool, _) => items.push(Item::Tools(vec![message])),
            _ => items.push(Item::Message(message)),
        }
    }
    items
}

fn transcript(
    ui: &mut Ui,
    pane: &mut ConversationPane,
    io: &ConversationIo<'_>,
    thread: &ThreadId,
    effects: &mut Vec<Effect>,
) {
    let history = io.cache.history.get(thread.as_str());
    egui::ScrollArea::vertical()
        .id_salt(("core-transcript", thread.as_str()))
        .stick_to_bottom(true)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add_space(SP_2);
            if io
                .cache
                .older
                .get(thread.as_str())
                .is_some_and(Option::is_some)
            {
                ui.vertical_centered(|ui| {
                    if ui
                        .add(ghost(icons::with_icon(
                            icons::ARROW_UP,
                            "Load earlier messages",
                        )))
                        .clicked()
                    {
                        effects.push(Effect::send(Action::History(thread.clone(), true)));
                    }
                });
            }
            let history = history.map(Vec::as_slice).unwrap_or_default();
            let questions = open_questions(io.cache, thread);
            let approvals = pending_approvals(io.cache, thread);
            if history.is_empty() && questions.is_empty() && approvals.is_empty() {
                empty_state(
                    ui,
                    "Start the conversation",
                    "Type a message below. Its replies appear here once the core commits them.",
                    None,
                );
            }
            for item in items(history) {
                match item {
                    Item::Message(message) => message_row(ui, pane, io, message, effects),
                    Item::Tools(tools) => tool_group(ui, pane, io, &tools, effects),
                }
                ui.add_space(SP_2);
            }
            for question in questions {
                question_card(ui, pane, thread, question, effects);
            }
            for approval in approvals {
                approval_card(ui, io.cache, approval, effects);
            }
            ui.add_space(SP_2);
        });
}

fn open_questions<'a>(cache: &'a Cache, thread: &ThreadId) -> Vec<&'a QuestionView> {
    cache
        .questions
        .values()
        .filter(|q| {
            q.answer.is_none()
                && (&q.requester.thread == thread
                    || matches!(&q.target, QuestionTarget::Lead { lead_thread, .. } if lead_thread == thread))
        })
        .collect()
}

fn pending_approvals<'a>(cache: &'a Cache, thread: &ThreadId) -> Vec<&'a ApprovalView> {
    cache
        .approvals
        .values()
        .filter(|a| &a.thread == thread && a.state == ApprovalState::Pending)
        .collect()
}

fn message_text<'a>(
    ui: &mut Ui,
    pane: &mut ConversationPane,
    io: &'a ConversationIo<'_>,
    message: &'a CommittedMessage,
    effects: &mut Vec<Effect>,
) -> Option<&'a str> {
    match &message.content {
        MessageContent::Inline { text } => Some(text),
        MessageContent::Body(body) => {
            if let Some(text) = io.cache.bodies.get(body.body_id.as_str()) {
                return Some(text);
            }
            if pane.requested_bodies.insert(body.body_id.to_string()) {
                effects.push(Effect::send(Action::Body(body.body_id.clone())));
            }
            ui.horizontal(|ui| {
                ui.label(muted(format!(
                    "Loading message ({})",
                    byte_size(body.total_bytes.get())
                )));
                if ui.add(ghost("Retry")).clicked() {
                    effects.push(Effect::send(Action::Body(body.body_id.clone())));
                }
            });
            None
        }
    }
}

fn byte_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} bytes")
    }
}

fn run_role(cache: &Cache, run: &RunId) -> Option<&'static str> {
    let run = cache.runs.get(run.as_str())?;
    run.parent_run.as_ref()?;
    Some(match run.role {
        RunRole::Orchestrator => "orchestrator",
        RunRole::Worker => "worker",
        RunRole::Planner => "planner",
        RunRole::Reviewer => "reviewer",
        RunRole::Advisor => "advisor",
        RunRole::Other => "subagent",
    })
}

fn message_row(
    ui: &mut Ui,
    pane: &mut ConversationPane,
    io: &ConversationIo<'_>,
    message: &CommittedMessage,
    effects: &mut Vec<Effect>,
) {
    ui.push_id(
        ("message", message.run.as_str(), message.index.get()),
        |ui| match message.role {
            MessageRole::User => {
                if let Some(text) = message_text(ui, pane, io, message, effects) {
                    user_bubble(ui, text, &[]);
                }
            }
            MessageRole::Assistant | MessageRole::Tool => {
                egui::Frame::new()
                    .inner_margin(egui::Margin::symmetric(SP_1 as i8, SP_1 as i8))
                    .show(ui, |ui| {
                        if let Some(role) = run_role(io.cache, &message.run) {
                            role_label(ui, role);
                        }
                        if let Some(text) = message_text(ui, pane, io, message, effects) {
                            render_markdown_with_base(
                                ui,
                                text,
                                &format!("msg-{}-{}", message.run, message.index.get()),
                                None,
                                None,
                            );
                        }
                    });
            }
        },
    );
}

fn tool_group(
    ui: &mut Ui,
    pane: &mut ConversationPane,
    io: &ConversationIo<'_>,
    tools: &[&CommittedMessage],
    effects: &mut Vec<Effect>,
) {
    let first = tools[0];
    let id = ui.make_persistent_id(("tools", first.run.as_str(), first.index.get()));
    let failed = tools.iter().any(|tool| match &tool.content {
        MessageContent::Inline { text } => ToolOutput::parse(text).failed(),
        MessageContent::Body(_) => false,
    });
    let count = tools.len();
    let mut title = if count == 1 {
        "1 tool output".to_owned()
    } else {
        format!("{count} tool outputs")
    };
    if failed {
        title.push_str(", some failed");
    }
    egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false)
        .show_header(ui, |ui| {
            let color = if failed {
                palette().ERROR_FG
            } else {
                palette().TEXT_MUTED
            };
            ui.label(
                RichText::new(icons::with_icon(icons::WRENCH, &title))
                    .size(FONT_SMALL)
                    .color(color),
            );
        })
        .body(|ui| {
            for tool in tools {
                ui.push_id(("tool", tool.run.as_str(), tool.index.get()), |ui| {
                    if let Some(role) = run_role(io.cache, &tool.run) {
                        role_label(ui, role);
                    }
                    if let Some(text) = message_text(ui, pane, io, tool, effects) {
                        tool_card(ui, text);
                    }
                });
                ui.add_space(SP_1);
            }
        });
}

/// What the shape of a committed tool output says about it. The core commits the output
/// alone, without the tool's name, input or error flag, so this reads the formats the
/// tools write: a shell result begins with its exit code, a change is a unified diff.
#[derive(Debug, PartialEq, Eq)]
enum ToolOutput<'a> {
    Shell { exit_code: i64, body: &'a str },
    Diff { added: usize, removed: usize },
    Text,
}

impl<'a> ToolOutput<'a> {
    fn parse(text: &'a str) -> Self {
        if let Some(rest) = text.strip_prefix("exit_code:") {
            let (code, body) = rest.split_once('\n').unwrap_or((rest, ""));
            if let Ok(exit_code) = code.trim().parse() {
                return Self::Shell { exit_code, body };
            }
        }
        let is_diff = text
            .lines()
            .any(|line| line.starts_with("@@ ") || line.starts_with("+++ "));
        if is_diff {
            let added = text
                .lines()
                .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
                .count();
            let removed = text
                .lines()
                .filter(|l| l.starts_with('-') && !l.starts_with("---"))
                .count();
            return Self::Diff { added, removed };
        }
        Self::Text
    }

    fn failed(&self) -> bool {
        matches!(self, Self::Shell { exit_code, .. } if *exit_code != 0)
    }
}

fn tool_card(ui: &mut Ui, text: &str) {
    let output = ToolOutput::parse(text);
    let (icon, title, meta) = match &output {
        ToolOutput::Shell { exit_code, .. } => {
            (icons::TERMINAL, "Shell", Some(format!("exit {exit_code}")))
        }
        ToolOutput::Diff { added, removed } => (
            icons::GIT_DIFF,
            "Change",
            Some(format!("+{added} −{removed}")),
        ),
        ToolOutput::Text => (icons::WRENCH, "Output", None),
    };
    let lines = text.lines().count();
    let first_line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    soft_frame(palette().SURFACE).show(ui, |ui| {
        ui.set_width(ui.available_width());
        let id = ui.id().with("tool-card");
        egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false)
            .show_header(ui, |ui| {
                let color = if output.failed() {
                    palette().ERROR_FG
                } else {
                    palette().TEXT
                };
                ui.label(RichText::new(icons::with_icon(icon, title)).color(color));
                let mut summary = String::new();
                if let Some(meta) = &meta {
                    summary.push_str(meta);
                    summary.push_str(" · ");
                }
                summary.push_str(&format!(
                    "{lines} line{}",
                    if lines == 1 { "" } else { "s" }
                ));
                ui.label(muted(summary).size(FONT_SMALL));
                ui.add(egui::Label::new(muted(first_line).size(FONT_SMALL).monospace()).truncate());
            })
            .body(|ui| match output {
                ToolOutput::Diff { .. } => diff_block(ui, text),
                ToolOutput::Shell { exit_code, body } => code_block(
                    ui,
                    body,
                    if exit_code == 0 {
                        palette().TEXT
                    } else {
                        palette().ERROR_FG
                    },
                ),
                ToolOutput::Text => code_block(ui, text, palette().TEXT),
            });
    });
}

fn question_card(
    ui: &mut Ui,
    pane: &mut ConversationPane,
    thread: &ThreadId,
    question: &QuestionView,
    effects: &mut Vec<Effect>,
) {
    ui.push_id(("question", question.id.as_str()), |ui| {
        request_card(ui, "Question for you", |ui| {
            ui.add(egui::Label::new(RichText::new(&question.title).strong()).wrap());
            ui.add(
                egui::Label::new(
                    RichText::new(if question.blocking {
                        "Work that depends on this answer is waiting"
                    } else {
                        "Optional answer"
                    })
                    .small()
                    .color(palette().TEXT_MUTED),
                )
                .wrap(),
            );
            let draft = pane.answers.entry(question.id.to_string()).or_default();
            for option in &question.options {
                if ui
                    .add(egui::Button::selectable(draft == option, option).wrap())
                    .clicked()
                {
                    *draft = option.clone();
                }
            }
            ui.add(
                egui::TextEdit::multiline(draft)
                    .id_salt("answer-input")
                    .hint_text("Type an answer (free text or pick an option)")
                    .desired_width(f32::INFINITY)
                    .desired_rows(2),
            );
            if ui
                .add_enabled(
                    !draft.trim().is_empty() && draft.len() <= MAX_ANSWER_BYTES,
                    egui::Button::new("Send answer"),
                )
                .on_disabled_hover_text(if draft.len() > MAX_ANSWER_BYTES {
                    "The answer is longer than 4096 bytes"
                } else {
                    "Type or pick an answer first"
                })
                .clicked()
            {
                effects.push(Effect::send(command(Command::Questions(
                    QuestionsCommand::AnswerUserQuestion(AnswerUserQuestion {
                        thread: thread.clone(),
                        question_id: question.id.clone(),
                        answer: draft.clone(),
                        question_revision: question.routing_revision,
                    }),
                ))));
            }
        });
    });
}

fn approval_card(ui: &mut Ui, cache: &Cache, approval: &ApprovalView, effects: &mut Vec<Effect>) {
    ui.push_id(("approval", approval.id.as_str()), |ui| {
        request_card(ui, "Tool approval needed", |ui| {
            ui.add(egui::Label::new(RichText::new(&approval.tool_name).strong()).wrap());
            let input = cache.approval_inputs.get(approval.id.as_str());
            match input {
                Some(input) => full_input(ui, input),
                None => {
                    full_input(ui, &approval.summary);
                    if approval.summary_truncated {
                        ui.colored_label(
                            palette().WARNING_FG,
                            "This summary is shortened. Do not approve before reading the full input.",
                        );
                    }
                    if ui
                        .add(ghost(icons::with_icon(icons::EYE, "Show full input")))
                        .clicked()
                    {
                        effects.push(Effect::send(Action::ApprovalInput(approval.clone())));
                    }
                }
            }
            ui.horizontal_wrapped(|ui| {
                for (label, decision) in [
                    ("Approve", ApprovalDecision::Approve),
                    ("Reject", ApprovalDecision::Deny),
                ] {
                    let enabled = decision == ApprovalDecision::Deny || input.is_some();
                    if ui
                        .add_enabled(enabled, egui::Button::new(label))
                        .on_disabled_hover_text("Read the full input first")
                        .clicked()
                    {
                        effects.push(Effect::send(command(Command::Approvals(
                            ApprovalsCommand::DecideToolApproval(DecideToolApproval {
                                approval_id: approval.id.clone(),
                                core_epoch: approval.core_epoch,
                                approval_revision: approval.revision,
                                decision,
                            }),
                        ))));
                    }
                }
            });
        });
    });
}

fn queue_band(ui: &mut Ui, cache: &Cache, thread: &ThreadId, effects: &mut Vec<Effect>) {
    let receipts: Vec<_> = cache
        .receipts
        .values()
        .filter(|r| {
            &r.thread == thread
                && matches!(
                    r.state,
                    InputReceiptState::Accepted
                        | InputReceiptState::Dropped
                        | InputReceiptState::Unknown
                )
        })
        .collect();
    if receipts.is_empty() {
        return;
    }
    soft_frame(palette().SURFACE).show(ui, |ui| {
        ui.set_width(ui.available_width());
        for receipt in receipts {
            ui.horizontal(|ui| {
                let (icon, text, color) = match receipt.state {
                    InputReceiptState::Accepted => {
                        (icons::QUEUE, "Queued follow-up", palette().TEXT_MUTED)
                    }
                    InputReceiptState::Dropped => {
                        (icons::WARNING, "Follow-up dropped", palette().WARNING_FG)
                    }
                    _ => (
                        icons::QUESTION,
                        "Follow-up state unknown",
                        palette().WARNING_FG,
                    ),
                };
                ui.label(
                    RichText::new(icons::with_icon(icon, text))
                        .size(FONT_SMALL)
                        .color(color),
                )
                .on_hover_text(receipt.submission_id.as_str());
                if receipt.state == InputReceiptState::Accepted
                    && let Some(run) = &receipt.run
                    && ui.add(ghost("Withdraw")).clicked()
                {
                    effects.push(Effect::send(command(Command::Conversation(
                        ConversationCommand::WithdrawFollowUp(WithdrawFollowUp {
                            thread: thread.clone(),
                            run: run.clone(),
                            submission_id: receipt.submission_id.clone(),
                        }),
                    ))));
                }
            });
        }
    });
    ui.add_space(SP_1);
}

fn composer(
    ui: &mut Ui,
    pane: &mut ConversationPane,
    io: &mut ConversationIo<'_>,
    project: &ProjectId,
    thread: &ThreadId,
    effects: &mut Vec<Effect>,
) {
    let running = is_running(io.cache, thread);
    let input_id = ui.id().with("composer-input");
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
    surface_frame(palette().SURFACE_RAISED)
        .corner_radius(R_2XL)
        .inner_margin(egui::vec2(SP_3, SP_2))
        .show(ui, |ui| {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(SP_2, SP_1);
                selectors(ui, pane, io, project, thread, effects);
                attachments(ui, io, project, thread, effects);
                let ime_id = ui.id().with("ime-composing");
                let ime = ime_composing(ui, ime_id);
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::BOTTOM), |ui| {
                        let draft_text = io
                            .drafts
                            .get(thread.as_str())
                            .map_or("", |d| d.text.as_str());
                        let has_attachments = io
                            .attachment_drafts
                            .get(thread.as_str())
                            .is_some_and(|files| !files.is_empty());
                        let can_send = io.can_send(thread.as_str())
                            && (!draft_text.trim().is_empty() || has_attachments);
                        let mut send = false;
                        if running
                            && ui
                                .add(stop_button())
                                .on_hover_text("Stop the run (Esc). Its subagents keep running.")
                                .clicked()
                        {
                            effects.push(Effect::send(stop(thread)));
                        }
                        let send_button = if running {
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
                                    ui.add(
                                        egui::TextEdit::multiline(&mut draft.text)
                                            .id(input_id)
                                            .hint_text("Message  (Shift+Enter: newline)")
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
                        let focused = input.has_focus();
                        let enter = focused
                            && !ime
                            && ui.input(|i| {
                                i.key_pressed(egui::Key::Enter)
                                    && !i.modifiers.shift
                                    && !i.modifiers.command
                            });
                        ui.data_mut(|d| d.insert_temp(ime_id, ime && focused));
                        if focused && running && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                            effects.push(Effect::send(stop(thread)));
                        }
                        if (send || enter) && can_send {
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
}

fn selectors(
    ui: &mut Ui,
    pane: &mut ConversationPane,
    io: &ConversationIo<'_>,
    project: &ProjectId,
    thread: &ThreadId,
    effects: &mut Vec<Effect>,
) {
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
        let sending = io.sending.contains(thread.as_str());
        ui.add_enabled_ui(!sending, |ui| {
            ui.menu_button(icons::with_icon(icons::PAPERCLIP, "Attach"), |ui| {
                for (label, intake) in [
                    ("Choose files…", Intake::ChooseFiles),
                    ("Paste image from clipboard", Intake::Clipboard),
                ] {
                    if ui.button(label).clicked() {
                        match pane.attachment_io.submit(thread.as_str(), intake, ui.ctx()) {
                            Ok(request) => effects.push(Effect::AttachRequested {
                                request,
                                project: project.clone(),
                                thread: thread.clone(),
                            }),
                            Err(error) => effects.push(Effect::Status(error)),
                        }
                        ui.close();
                    }
                }
            });
        });
    });
}

fn attachments(
    ui: &mut Ui,
    io: &ConversationIo<'_>,
    project: &ProjectId,
    thread: &ThreadId,
    effects: &mut Vec<Effect>,
) {
    let files = io
        .attachment_drafts
        .get(thread.as_str())
        .map(Vec::as_slice)
        .unwrap_or_default();
    let loading = io
        .loading_attachments
        .get(thread.as_str())
        .copied()
        .unwrap_or(0);
    if files.is_empty() && loading == 0 {
        return;
    }
    ui.horizontal_wrapped(|ui| {
        for file in files {
            let state = if file.reference.is_some() {
                "ready"
            } else if file.uploading {
                "uploading"
            } else {
                "needs retry"
            };
            crate::theme::widgets::badge(
                ui,
                format!(
                    "{} · {} · {state}",
                    file.file.filename,
                    byte_size(file.file.bytes.len() as u64)
                ),
                if file.reference.is_some() || file.uploading {
                    palette().TEXT
                } else {
                    palette().WARNING_FG
                },
                palette().SURFACE,
            );
        }
        if loading > 0 {
            ui.label(muted("Reading files…").size(FONT_SMALL));
        }
        let pending = files
            .iter()
            .any(|file| file.reference.is_none() && !file.uploading && !file.uncertain);
        if pending && ui.add(ghost("Retry upload")).clicked() {
            effects.push(Effect::RetryAttachments {
                project: project.clone(),
                thread: thread.clone(),
            });
        }
        if ui
            .add_enabled(
                io.can_discard_attachments(thread.as_str()),
                ghost(icons::with_icon(icons::X, "Discard")),
            )
            .clicked()
        {
            effects.push(Effect::DiscardAttachments(thread.clone()));
        }
    });
}

fn submit(
    pane: &mut ConversationPane,
    io: &ConversationIo<'_>,
    project: &ProjectId,
    thread: &ThreadId,
    effects: &mut Vec<Effect>,
) {
    let Some(draft) = io.drafts.get(thread.as_str()) else {
        return;
    };
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
    let exists = io.cache.threads.contains_key(thread.as_str());
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
            skills: vec![],
            attachments,
            model_preference: None,
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
}

fn status_strip(ui: &mut Ui, cache: &Cache, thread: &ThreadId) {
    ui.horizontal(|ui| {
        ui.add_space(SP_2);
        let phase = thread_phase(cache, thread);
        phase_circle(ui, phase);
        let state = match cache.threads.get(thread.as_str()).map(|t| t.state) {
            None => "New",
            Some(ThreadState::Idle) => "Idle",
            Some(ThreadState::Running) => "Running",
            Some(ThreadState::WaitingForUser) => "Waiting for you",
            Some(ThreadState::WaitingForApproval) => "Waiting for approval",
            Some(ThreadState::Failed) => "Failed",
        };
        ui.label(muted(state).size(FONT_SMALL));
        let subagents = cache
            .runs
            .values()
            .filter(|r| &r.thread == thread && r.parent_run.is_some())
            .filter(|r| {
                matches!(
                    r.phase,
                    RunPhase::Pending
                        | RunPhase::Running
                        | RunPhase::WaitingForUser
                        | RunPhase::WaitingForApproval
                )
            })
            .count();
        if subagents > 0 {
            ui.label(
                muted(icons::with_icon(
                    icons::ROBOT,
                    format!(
                        "{subagents} subagent{} running",
                        if subagents == 1 { "" } else { "s" }
                    ),
                ))
                .size(FONT_SMALL),
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shell_result_is_read_from_its_exit_code_line() {
        assert_eq!(
            ToolOutput::parse("exit_code: 2\n--- stderr ---\nboom"),
            ToolOutput::Shell {
                exit_code: 2,
                body: "--- stderr ---\nboom"
            }
        );
        assert!(ToolOutput::parse("exit_code: 1\n").failed());
        assert!(!ToolOutput::parse("exit_code: 0\nok").failed());
    }

    #[test]
    fn a_unified_diff_is_counted_without_its_file_headers() {
        let diff = "--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n-old\n+new\n+more\n context";
        assert_eq!(
            ToolOutput::parse(diff),
            ToolOutput::Diff {
                added: 2,
                removed: 1
            }
        );
    }

    #[test]
    fn plain_output_is_neither_a_shell_result_nor_a_diff() {
        assert_eq!(ToolOutput::parse("exit_code: x\nhello"), ToolOutput::Text);
        assert_eq!(ToolOutput::parse("just text"), ToolOutput::Text);
    }
}
