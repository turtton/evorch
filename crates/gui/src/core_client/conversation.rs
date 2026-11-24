//! The conversation tab of the core client, laid out as the embedded workbench's
//! conversation pane: a header, the committed transcript, the cards that wait for the
//! person, and the composer at the bottom. It draws only what the core has committed.
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use egui::{RichText, Ui};
use workbench_protocol::namespaces::execution::{GoalPhase, GoalView};
use workbench_protocol::namespaces::{
    ApprovalsCommand, ConversationCommand, ExecutionCommand, QuestionsCommand,
};
use workbench_protocol::{commands::*, ids::*, views::*};
use workspace_ui::ThreadRunPhase;

use super::cache::Cache;
use super::connection::Action;
use super::features::execution::ExecutionBand;
use super::features::lanes::{CardChoice, CardPlace, LaneCardAction, LaneCardView, lane_body};
use super::features::pull_requests::PullRequestBar;
use super::features::{FeatureId, Selection};
use super::run_tabs::RunTab;
use super::shell::{CoreTab, Effect};
use super::transcript::{self, Scope, TranscriptView, byte_size};
use super::{ClientAttachmentDraft, Draft};
use crate::attachments::{AttachmentIo, Intake};
use crate::panes::goal_widgets::{GoalBandView, GoalCheckRow, GoalPhaseView, progress_band};
use crate::panes::phase_indicator::phase_circle;
use crate::panes::pull_request_widgets;
use crate::panes::transcript_widgets::{
    full_input, ime_composing, request_card, run_transcript_empty, run_transcript_title,
    stop_button as shared_stop_button,
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
    /// The message bodies asked of the projection numbered `projection`.
    requested_bodies: BTreeSet<String>,
    projection: u64,
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
            projection: 0,
        }
    }
}

impl ConversationPane {
    /// A new projection (a reconnect or resync) holds none of the bodies read before,
    /// so its messages ask for them again.
    pub(super) fn follow_projection(&mut self, projection: u64) {
        if self.projection != projection {
            self.projection = projection;
            self.requested_bodies.clear();
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
    /// What relative links in the transcripts resolve against: the selected
    /// thread's working tree on this machine.
    pub link_base: Option<&'a Path>,
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

/// What the areas draw into the conversation: the execution feature's goal and plan band
/// above the composer, the lanes feature's card of a lane thread under the header, and the
/// pull requests feature's bar above the composer. Each is drawn only for the thread it was
/// read for.
pub struct Bands<'a> {
    pub band: Option<ExecutionBand<'a>>,
    pub lane: Option<(&'a ThreadId, LaneCardView<'a>)>,
    pub pull_request: Option<PullRequestBar<'a>>,
}

/// `root` is the thread's current root run, as its detail names it.
pub fn show(
    ui: &mut Ui,
    pane: &mut ConversationPane,
    io: &mut ConversationIo<'_>,
    selection: &Selection,
    Bands {
        band,
        lane,
        pull_request,
    }: Bands<'_>,
    root: Option<&RunId>,
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
        let activity = activity(io.cache, &thread, root);
        ui.add_enabled_ui(io.connected, |ui| {
            header(
                ui,
                io,
                &project,
                &thread,
                activity,
                pane.composer_role,
                effects,
            );
            if let Some((_, card)) = lane.filter(|(lane_thread, _)| *lane_thread == &thread) {
                lane_card(ui, io.cache, &card, pane.composer_role, effects);
            }
            let conversation_height = ui.available_height();
            let details_max = conversation_height * 0.3;
            egui::Panel::bottom(ui.id().with("composer"))
                .resizable(false)
                .show_separator_line(false)
                .frame(egui::Frame::NONE.inner_margin(egui::Margin::symmetric(0, SP_1 as i8)))
                .show(ui, |ui| {
                    let band = band.filter(|band| band.thread == &thread);
                    // The band sits flush on the composer, as in the embedded workbench.
                    let vertical_spacing = ui.spacing().item_spacing.y;
                    ui.spacing_mut().item_spacing.y = 0.0;
                    progress(ui, io.cache, &thread, band, details_max, effects);
                    composer(ui, pane, io, &project, &thread, activity, effects);
                    ui.spacing_mut().item_spacing.y = vertical_spacing;
                    status_strip(ui, io.cache, &thread, activity);
                });
            // The bar sits above the composer, as in the embedded workbench.
            if let Some(bar) = pull_request.filter(|bar| bar.thread == &thread)
                && let Some(choice) = pull_request_widgets::bar(ui, &bar.view, conversation_height)
            {
                effects.push(Effect::PullRequest(bar.action(choice)));
            }
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE)
                .show(ui, |ui| thread_transcript(ui, pane, io, &thread, effects));
        });
    });
}

/// One run tab: the committed messages of a single run, as the embedded workbench's
/// transcript tab shows a subagent.
pub fn run_tab(
    ui: &mut Ui,
    pane: &mut ConversationPane,
    cache: &Cache,
    connected: bool,
    base: Option<&Path>,
    tab: Option<&RunTab>,
    effects: &mut Vec<Effect>,
) {
    let Some(tab) = tab else {
        pane_root(ui, "Transcript", |ui| run_transcript_empty(ui, None));
        return;
    };
    pane_root(ui, &tab.title, |ui| {
        if !connected {
            ui.colored_label(
                palette().WARNING_FG,
                "Not connected to the core. Nothing runs in this window in its place.",
            );
        }
        ui.add_enabled_ui(connected, |ui| {
            let name = tab.name.as_deref();
            run_transcript_title(ui, tab.run.as_str(), name);
            transcript::transcript(
                ui,
                &mut pane.requested_bodies,
                TranscriptView {
                    cache,
                    scope: Scope::Run {
                        thread: &tab.thread,
                        run: &tab.run,
                    },
                    base,
                },
                |ui| run_transcript_empty(ui, name),
                |_, _| {},
                effects,
            );
        });
    });
}

fn thread_phase(cache: &Cache, thread: &ThreadId, activity: Activity) -> Option<ThreadRunPhase> {
    if activity.stopped {
        return Some(ThreadRunPhase::Stopped);
    }
    let state = cache.threads.get(thread.as_str())?.state;
    Some(match state {
        ThreadState::Idle => return None,
        ThreadState::Running => ThreadRunPhase::Running,
        ThreadState::WaitingForUser | ThreadState::WaitingForApproval => ThreadRunPhase::Waiting,
        ThreadState::Failed => ThreadRunPhase::Error,
    })
}

/// What the runs of a thread leave the person to stop.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Activity {
    /// A root run of the thread works, or waits on the person: Stop stops it.
    pub root_active: bool,
    /// The current root was stopped while subagents still run: Stop subagents
    /// stops them.
    pub stopped: bool,
    pub running_children: usize,
}

fn is_active(phase: RunPhase) -> bool {
    matches!(
        phase,
        RunPhase::Pending
            | RunPhase::Running
            | RunPhase::WaitingForUser
            | RunPhase::WaitingForApproval
    )
}

/// `root` is the thread's current root run; without it, the thread's only run without
/// a parent is.
/// Only the current root's own subagents count, as the core stops (and the embedded
/// workbench counts) the live descendants of the thread's bound root alone.
pub(super) fn activity(cache: &Cache, thread: &ThreadId, root: Option<&RunId>) -> Activity {
    let runs = || cache.runs.values().filter(|run| &run.thread == thread);
    let root_active = runs().any(|run| run.parent_run.is_none() && is_active(run.phase));
    let current = current_root(cache, thread, root);
    let running_children = current.map_or(0, |current| {
        cache
            .runs
            .values()
            .filter(|run| is_active(run.phase) && descends_from(cache, run, &current.id))
            .count()
    });
    Activity {
        root_active,
        stopped: !root_active
            && running_children > 0
            && current.is_some_and(|run| run.phase == RunPhase::Cancelled),
        running_children,
    }
}

/// The thread's current root run: `root`, the one its detail names, or without it the
/// thread's only run without a parent.
pub(super) fn current_root<'c>(
    cache: &'c Cache,
    thread: &ThreadId,
    root: Option<&RunId>,
) -> Option<&'c RunSummary> {
    match root {
        Some(root) => cache.runs.get(root.as_str()),
        None => {
            let mut roots = cache
                .runs
                .values()
                .filter(|run| &run.thread == thread && run.parent_run.is_none());
            roots.next().filter(|_| roots.next().is_none())
        }
    }
}

/// Whether `root` is among the ancestors of `run`, walking the parents the cache knows.
fn descends_from(cache: &Cache, run: &RunSummary, root: &RunId) -> bool {
    let mut parent = run.parent_run.as_ref();
    // A parent chain is at most as long as the runs known; this also ends a cycle.
    for _ in 0..cache.runs.len() {
        match parent {
            Some(id) if id == root => return true,
            Some(id) => {
                parent = cache
                    .runs
                    .get(id.as_str())
                    .and_then(|run| run.parent_run.as_ref())
            }
            None => return false,
        }
    }
    false
}

fn header(
    ui: &mut Ui,
    io: &ConversationIo<'_>,
    project: &ProjectId,
    thread: &ThreadId,
    activity: Activity,
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
                phase_circle(ui, thread_phase(io.cache, thread, activity));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.menu_button(icons::DOTS_THREE, |ui| {
                        if ui.button("Refresh history").clicked() {
                            effects.push(Effect::send(Action::History(thread.clone(), None, false)));
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
                    if activity.root_active {
                        if ui
                            .add(stop_button())
                            .on_hover_text(
                                "Stop the run. Its subagents keep running until stopped separately.",
                            )
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

/// The lane card of a lane's conversation, under its header as in the embedded workbench.
/// Resuming held notices goes through the lanes feature, which knows the lead's root run;
/// continuing the lead is the same command as the header's Continue, for the lead.
fn lane_card(
    ui: &mut Ui,
    cache: &Cache,
    card: &LaneCardView<'_>,
    role: ComposerRole,
    effects: &mut Vec<Effect>,
) {
    let lead_project = cache
        .threads
        .get(card.lead.as_str())
        .map(|lead| lead.project.clone());
    let choice = soft_frame(palette().SURFACE)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            lane_body(
                ui,
                card,
                CardPlace::Conversation {
                    can_continue: lead_project.is_some(),
                },
            )
        })
        .inner;
    match choice {
        Some(CardChoice::ResumeNotifications) => {
            effects.push(Effect::Lane(LaneCardAction::ResumeNotifications {
                lane: card.summary.lane_id.clone(),
            }));
        }
        Some(CardChoice::ContinueLead) => {
            if let Some(project) = lead_project {
                effects.push(Effect::send(command(Command::Conversation(
                    ConversationCommand::ContinueChat(ContinueChat {
                        project,
                        thread: card.lead.clone(),
                        composer_role: role,
                        model_preference: None,
                    }),
                ))));
            }
        }
        None => {}
    }
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

fn thread_transcript(
    ui: &mut Ui,
    pane: &mut ConversationPane,
    io: &ConversationIo<'_>,
    thread: &ThreadId,
    effects: &mut Vec<Effect>,
) {
    let questions = open_questions(io.cache, thread);
    let approvals = pending_approvals(io.cache, thread);
    let waiting = !questions.is_empty() || !approvals.is_empty();
    // While older pages exist, the thread may have messages that are not read yet.
    let older = io
        .cache
        .older
        .get(thread.as_str())
        .is_some_and(Option::is_some);
    let answers = &mut pane.answers;
    transcript::transcript(
        ui,
        &mut pane.requested_bodies,
        TranscriptView {
            cache: io.cache,
            scope: Scope::Thread(thread),
            base: io.link_base,
        },
        |ui| {
            if !waiting && !older {
                empty_state(
                    ui,
                    "Start the conversation",
                    "Type a message below. Its replies appear here once the core commits them.",
                    None,
                );
            }
        },
        |ui, effects| {
            for question in questions {
                question_card(ui, answers, thread, question, effects);
            }
            for approval in approvals {
                approval_card(ui, io.cache, approval, effects);
            }
        },
        effects,
    );
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

fn question_card(
    ui: &mut Ui,
    answers: &mut BTreeMap<String, String>,
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
            let draft = answers.entry(question.id.to_string()).or_default();
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

/// The queued follow-ups, the goal and the plan of `thread` in one band above
/// the composer, as the embedded workbench draws them.
fn progress(
    ui: &mut Ui,
    cache: &Cache,
    thread: &ThreadId,
    band: Option<ExecutionBand<'_>>,
    details_max: f32,
    effects: &mut Vec<Effect>,
) {
    let receipts = queued_receipts(cache, thread);
    let goal = band
        .and_then(|band| band.goal)
        .map(|goal| goal_view(thread, goal));
    let queue = (!receipts.is_empty()).then_some(|ui: &mut Ui| queue_rows(ui, &receipts, thread));
    let output = progress_band(
        ui,
        thread.as_str(),
        goal.as_ref(),
        queue,
        details_max,
        band.map(|band| band.plan),
    );
    effects.extend(output.queue_action);
    effects.extend(output.action.map(Effect::Goal));
    if output.open_plan {
        effects.push(Effect::Open(CoreTab::Plan));
    }
}

fn goal_view<'a>(thread: &'a ThreadId, goal: &'a GoalView) -> GoalBandView<'a> {
    GoalBandView {
        thread_id: thread.as_str(),
        goal_id: goal.goal.as_str(),
        objective: &goal.objective,
        phase: match goal.phase {
            GoalPhase::Working => GoalPhaseView::Working,
            GoalPhase::Checking => GoalPhaseView::Checking,
            GoalPhase::Reviewing => GoalPhaseView::Reviewing,
            GoalPhase::Repairing => GoalPhaseView::Repairing,
            GoalPhase::Complete => GoalPhaseView::Complete,
            GoalPhase::Blocked => GoalPhaseView::Blocked,
        },
        review_enabled: goal.review_enabled,
        checks_paused: goal.checks_paused,
        work_stopped: goal.work_stopped,
        reason: goal.reason.as_deref(),
        review_round: goal.review_round,
        max_review_rounds: goal.max_review_rounds,
        criteria: &goal.criteria,
        checks: goal
            .checks
            .iter()
            .filter_map(|check| {
                Some(GoalCheckRow {
                    criterion: usize::try_from(check.criterion).ok()?,
                    met: check.met,
                    evidence: &check.evidence,
                })
            })
            .collect(),
        findings: &goal.findings,
        original_request: None,
        usage: None,
    }
}

fn queued_receipts<'a>(cache: &'a Cache, thread: &ThreadId) -> Vec<&'a InputReceipt> {
    cache
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
        .collect()
}

fn queue_rows(ui: &mut Ui, receipts: &[&InputReceipt], thread: &ThreadId) -> Option<Effect> {
    let mut withdraw = None;
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
                withdraw = Some(Effect::send(command(Command::Conversation(
                    ConversationCommand::WithdrawFollowUp(WithdrawFollowUp {
                        thread: thread.clone(),
                        run: run.clone(),
                        submission_id: receipt.submission_id.clone(),
                    }),
                ))));
            }
        });
    }
    withdraw
}

fn composer(
    ui: &mut Ui,
    pane: &mut ConversationPane,
    io: &mut ConversationIo<'_>,
    project: &ProjectId,
    thread: &ThreadId,
    activity: Activity,
    effects: &mut Vec<Effect>,
) {
    let can_stop = activity.root_active || activity.stopped;
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
                        if can_stop
                            && shared_stop_button(ui, activity.stopped, activity.running_children)
                                .clicked()
                        {
                            effects.push(Effect::send(stop(thread)));
                        }
                        let send_button = if activity.root_active {
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
                        if focused && can_stop && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
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

fn status_strip(ui: &mut Ui, cache: &Cache, thread: &ThreadId, activity: Activity) {
    ui.horizontal(|ui| {
        ui.add_space(SP_2);
        let phase = thread_phase(cache, thread, activity);
        phase_circle(ui, phase);
        let state = match cache.threads.get(thread.as_str()).map(|t| t.state) {
            _ if activity.stopped => "Stopped",
            None => "New",
            Some(ThreadState::Idle) => "Idle",
            Some(ThreadState::Running) => "Running",
            Some(ThreadState::WaitingForUser) => "Waiting for you",
            Some(ThreadState::WaitingForApproval) => "Waiting for approval",
            Some(ThreadState::Failed) => "Failed",
        };
        ui.label(muted(state).size(FONT_SMALL));
        let subagents = activity.running_children;
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

    fn cache(runs: &[(&str, Option<&str>, &str)]) -> Cache {
        let mut cache = Cache::default();
        for (id, parent, phase) in runs {
            let run: RunSummary = serde_json::from_value(serde_json::json!({
                "id": id, "thread": "thr", "role": "worker", "phase": phase,
                "revision": "1", "parent_run": parent,
            }))
            .unwrap();
            cache.runs.insert((*id).into(), run);
        }
        cache
    }

    fn of(cache: &Cache, root: Option<&str>) -> Activity {
        let root = root.map(|root| RunId::new(root).unwrap());
        activity(cache, &ThreadId::new("thr").unwrap(), root.as_ref())
    }

    #[test]
    fn a_cancelled_root_with_a_running_child_is_stopped() {
        let cache = cache(&[
            ("root", None, "cancelled"),
            ("child", Some("root"), "running"),
        ]);
        assert_eq!(
            of(&cache, None),
            Activity {
                root_active: false,
                stopped: true,
                running_children: 1
            }
        );
    }

    #[test]
    fn only_an_active_root_offers_stop() {
        let waiting = cache(&[("root", None, "waiting_for_user")]);
        assert!(of(&waiting, None).root_active);
        // A finished root leaves its background subagents running, and nothing to stop.
        let done = cache(&[("root", None, "done"), ("child", Some("root"), "running")]);
        let activity = of(&done, None);
        assert!(!activity.root_active && !activity.stopped);
        assert_eq!(activity.running_children, 1);
    }

    // Given: an earlier cancelled root and the current root that finished, with a
    //        subagent of the current root still running
    // Then: the detail's root decides, so the thread is not stopped; without the
    //       detail neither root is the current one
    #[test]
    fn the_detail_root_is_the_current_root() {
        let cache = cache(&[
            ("old", None, "cancelled"),
            ("new", None, "done"),
            ("child", Some("new"), "running"),
        ]);
        assert!(!of(&cache, Some("new")).stopped);
        assert!(!of(&cache, None).stopped);
        let mut cancelled = cache.clone();
        cancelled.runs.get_mut("new").unwrap().phase = RunPhase::Cancelled;
        assert!(of(&cancelled, Some("new")).stopped);
    }

    // Given: the current root "new" was stopped, an earlier root's subagent still
    //        runs, and so do the current root's grandchild and a stale child of it
    //        that finished
    // Then: only the current root's live descendants count, as the core stops those
    //       alone; with none left the thread is not offered Stop subagents
    #[test]
    fn only_the_current_roots_live_descendants_count() {
        let mut runs = cache(&[
            ("old", None, "cancelled"),
            ("old-child", Some("old"), "running"),
            ("new", None, "cancelled"),
        ]);
        let other = of(&runs, Some("new"));
        assert_eq!((other.stopped, other.running_children), (false, 0));
        let mut own = cache(&[
            ("child", Some("new"), "done"),
            ("grandchild", Some("child"), "running"),
        ]);
        runs.runs.append(&mut own.runs);
        let own = of(&runs, Some("new"));
        assert_eq!((own.stopped, own.running_children), (true, 1));
    }
}
