//! Agent トランスクリプトペインの描画。

use egui::Color32;
use workspace_ui::ThreadRunPhase;

use crate::model::composer::ComposerModel;
use crate::model::transcript::{TranscriptEntry, TranscriptModel};
use crate::panes::agents::AgentsAction;
use crate::panes::composer::ComposerAction;
use crate::panes::sidebar::SidebarAction;
use crate::theme::icons::{self, with_icon};
use crate::theme::text::medium;
use crate::theme::tokens::*;
use crate::theme::widgets::{empty_state, pane_root, soft_frame};

mod agent_message;
mod compaction;
mod goal;
pub use goal::GoalAction;
mod branch;
pub use branch::BranchContext;
mod composer_panel;
mod header;
mod pull_request_bar;
pub use pull_request_bar::{
    PullRequestBarContext, glyph_style as pull_request_glyph_style, max_bar_height,
};
mod lane;
use header::header_strip;
pub(crate) mod async_layout;
mod queue;
mod sandbox_review;
mod thinking;
mod unreadable;
mod virtual_list;
pub(crate) use virtual_list::mark_async_layout;
#[doc(hidden)]
pub use virtual_list::{set_transcript_virtualization_for_tests, transcript_layout_for_tests};

#[derive(Debug, Clone, Copy)]
pub struct AgentIdentity<'a> {
    pub run_id: &'a str,
    pub name: Option<&'a str>,
    pub role: Option<&'a str>,
    pub ledger: &'a [storage::RunLedgerEntry],
}

/// The lane the shown thread is, with the thread that opened it.
pub struct LaneContext<'a> {
    pub detail: &'a crate::model::lanes::LaneDetail,
    pub origin: Option<&'a workspace_ui::ThreadRecord>,
    /// Whether this window runs the runtime's durable lane delivery (words the routing line).
    pub delivery_bound: bool,
}

/// The lanes a shown lead thread manages and the questions waiting for its answer.
pub struct LeadContext<'a> {
    pub thread_id: &'a str,
    pub lanes: Vec<&'a crate::model::lanes::LaneDetail>,
    pub waiting_questions: usize,
}

/// Operations a delivery notice offers; the runtime decides what each one does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaneAction {
    /// The user's explicit resume of held lane notices for a lead thread.
    ResumeDelivery { lead: String },
    /// Continue a stopped lead so its held notices can be delivered.
    ContinueLead { lead: String },
}

/// 会話ペインが描画される文脈です。
pub struct ConversationContext<'a> {
    pub todo: Option<&'a event_bus::ThreadTodoSnapshot>,
    pub goal: Option<&'a event_bus::ThreadGoalSnapshot>,
    pub requests: Option<super::requests::ConversationRequests<'a>>,
    pub task_rows: &'a [crate::model::tasks::TaskRow],
    pub phase_unread: bool,
    pub has_project: bool,
    pub active_thread_title: Option<&'a str>,
    /// Directories the active thread's runs may read, never write (ADR 0037).
    pub extra_read_dirs: &'a [std::path::PathBuf],
    pub parent_thread: Option<&'a workspace_ui::ThreadRecord>,
    pub child_threads: Vec<&'a workspace_ui::ThreadRecord>,
    /// Present when the shown thread is a lane whose ledger record has been read.
    pub lane: Option<LaneContext<'a>>,
    /// Present when the shown thread leads lanes.
    pub lead: Option<LeadContext<'a>>,
    pub thread_metrics: Option<crate::model::telemetry::ThreadMetrics>,
    pub phase: Option<ThreadRunPhase>,
    pub model_picker: crate::panes::model_picker::ModelPickerContext<'a>,
    pub sandbox_picker: crate::panes::composer::SandboxPickerContext,
    /// Present only while showing a thread conversation.
    pub branch: Option<BranchContext<'a>>,
    /// The thread's linked pull request, shown above the composer (also in a subagent's
    /// transcript: the pull request belongs to the thread).
    pub pull_request: Option<PullRequestBarContext<'a>>,
    /// Candidate chips by the tool call that produced them, drawn under its card.
    pub pull_request_chips: Option<(&'a str, &'a crate::panes::pull_request_chip::ChipsByCall)>,
}

/// Agent 会話ペインから発生するアクションです。
pub enum AgentPaneAction {
    Goal(GoalAction),
    Agents(AgentsAction),
    Sidebar(SidebarAction),
    FocusPanel(&'static str),
    OpenDiagnostics,
    OpenContext,
    Composer(ComposerAction),
    ModelPreference(Option<workspace_ui::ModelPreference>),
    Request(super::requests::RequestAction),
    Lane(LaneAction),
    PullRequest(crate::model::pull_requests::PullRequestAction),
}

/// トランスクリプトモデルを egui 上に描画します。
pub fn agent_pane(
    ui: &mut egui::Ui,
    model: &TranscriptModel,
    identity: Option<AgentIdentity<'_>>,
    ctx: ConversationContext<'_>,
    composer: &mut ComposerModel,
    picker_state: &mut crate::model::model_picker::ModelPickerState,
) -> Option<AgentPaneAction> {
    agent_pane_with_repo_root(ui, model, identity, ctx, composer, picker_state, None)
}

pub fn agent_pane_with_repo_root(
    ui: &mut egui::Ui,
    model: &TranscriptModel,
    identity: Option<AgentIdentity<'_>>,
    ctx: ConversationContext<'_>,
    composer: &mut ComposerModel,
    picker_state: &mut crate::model::model_picker::ModelPickerState,
    repo_root: Option<&std::path::Path>,
) -> Option<AgentPaneAction> {
    let mut ctx = ctx;
    pane_root(ui, "Conversation", |ui| {
        let mut action = None;
        header_strip(ui, &identity, &ctx, &mut action);
        let conversation_height = ui.available_height();
        composer_panel::composer_panel(
            ui,
            &ctx,
            composer,
            picker_state,
            identity.is_some(),
            &mut action,
        );
        if let Some(bar) = &ctx.pull_request {
            pull_request_bar::show(ui, bar, conversation_height, &mut action);
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
                if model.visible_entries().is_empty()
                    && ctx
                        .requests
                        .as_ref()
                        .is_none_or(|requests| requests.is_empty())
                    && identity.is_none_or(|identity| identity.ledger.is_empty())
                {
                    empty_state_body(ui, &ctx, &mut action);
                } else {
                    let mut branch_action = None;
                    let mut chip_action = None;
                    let mut chips = ctx.pull_request_chips.map(|(thread_id, by_call)| {
                        crate::panes::pull_request_chip::PullRequestChips {
                            thread_id,
                            by_call,
                            action: &mut chip_action,
                        }
                    });
                    if let Some(request) = run_detail_body(
                        ui,
                        model,
                        (identity, ctx.task_rows),
                        repo_root,
                        ctx.requests.as_mut(),
                        (ctx.branch.as_ref(), &mut branch_action),
                        chips.as_mut(),
                    ) {
                        action = Some(AgentPaneAction::Request(request));
                    }
                    if let Some(chip_action) = chip_action {
                        action = Some(AgentPaneAction::PullRequest(chip_action));
                    }
                    if let Some(branch_action) = branch_action {
                        action = Some(AgentPaneAction::Sidebar(branch_action));
                    }
                }
            });
        action
    })
}

fn empty_state_body(
    ui: &mut egui::Ui,
    ctx: &ConversationContext<'_>,
    action: &mut Option<AgentPaneAction>,
) {
    if !ctx.has_project {
        if empty_state(
            ui,
            "No project selected",
            "Add a repository in the Projects panel to begin.",
            Some("Go to Projects"),
        ) {
            *action = Some(AgentPaneAction::FocusPanel("sidebar-main"));
        }
    } else if ctx.active_thread_title.is_none() {
        if empty_state(
            ui,
            "No thread selected",
            "Start a thread to open a conversation.",
            Some("Start a thread"),
        ) {
            *action = Some(AgentPaneAction::Sidebar(SidebarAction::CreateThread));
        }
    } else {
        empty_state(
            ui,
            "No messages yet",
            "Type a message below, or /goal <text> to track an objective.",
            None,
        );
    }
}

/// Index of the last entry about each shell job; only that card previews
/// the job's live output.
fn latest_shell_job_entries(
    entries: &[TranscriptEntry],
    jobs: &crate::model::transcript::shell_jobs::ShellJobs,
) -> std::collections::HashMap<crate::model::transcript::shell_jobs::ShellJobKey, usize> {
    entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            crate::panes::transcript_tool::resolved_shell_job_key(entry, jobs)
                .map(|job| (job, index))
        })
        .collect()
}

/// Only hash mutation counters, not large markdown/tool-output strings. A
/// parent's layout also depends on its nested children and live shell state.
fn row_revision(
    model: &TranscriptModel,
    indices: impl Iterator<Item = usize>,
    latest_jobs: &std::collections::HashMap<
        crate::model::transcript::shell_jobs::ShellJobKey,
        usize,
    >,
    layout_revision: u64,
) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut revision = std::hash::DefaultHasher::new();
    layout_revision.hash(&mut revision);
    for index in indices {
        let id = model.visible_entry_id(index);
        id.hash(&mut revision);
        model.entry_revision(id).hash(&mut revision);
        model.thinking_is_streaming(id).hash(&mut revision);
        let job = crate::panes::transcript_tool::resolved_shell_job_key(
            &model.visible_entries()[index],
            model.shell_jobs(),
        );
        job.hash(&mut revision);
        if let Some(key) = job {
            model
                .shell_jobs()
                .get_key(&key)
                .map(|job| job.revision())
                .hash(&mut revision);
            (latest_jobs.get(&key) == Some(&index)).hash(&mut revision);
        }
    }
    revision.finish()
}

pub fn transcript_body(ui: &mut egui::Ui, model: &TranscriptModel) {
    transcript_body_with_repo_root(ui, model, None);
}

pub fn transcript_body_with_repo_root(
    ui: &mut egui::Ui,
    model: &TranscriptModel,
    repo_root: Option<&std::path::Path>,
) {
    transcript_body_with_agents(ui, model, repo_root, &[]);
}

/// A run transcript whose run IDs resolve to the names in `agents`.
pub fn transcript_body_with_agents(
    ui: &mut egui::Ui,
    model: &TranscriptModel,
    repo_root: Option<&std::path::Path>,
    agents: &[crate::model::tasks::TaskRow],
) {
    run_detail_body(
        ui,
        model,
        (None, agents),
        repo_root,
        None,
        (None, &mut None),
        None,
    );
}

fn run_detail_body(
    ui: &mut egui::Ui,
    model: &TranscriptModel,
    context: (Option<AgentIdentity<'_>>, &[crate::model::tasks::TaskRow]),
    repo_root: Option<&std::path::Path>,
    requests: Option<&mut super::requests::ConversationRequests<'_>>,
    (branch, branch_action): (Option<&BranchContext<'_>>, &mut Option<SidebarAction>),
    mut chips: Option<&mut crate::panes::pull_request_chip::PullRequestChips<'_>>,
) -> Option<super::requests::RequestAction> {
    let (identity, task_rows) = context;
    let pane_id = ui.id();
    let model_id = if let Some(branch) = branch {
        pane_id.with(("thread", &branch.thread.id))
    } else if let Some(identity) = identity {
        pane_id.with(("run", identity.run_id))
    } else if let Some(run_id) = model.entries().iter().find_map(|entry| match entry {
        TranscriptEntry::Message { run_id, .. }
        | TranscriptEntry::Reasoning { run_id, .. }
        | TranscriptEntry::Tool { run_id, .. }
        | TranscriptEntry::SandboxReview { run_id, .. } => run_id.as_deref(),
        TranscriptEntry::TurnEnd { run_id, .. } => Some(run_id.as_str()),
        _ => None,
    }) {
        pane_id.with(("run", run_id))
    } else {
        // Standalone transcripts may have no thread or run identity.
        pane_id.with(("transcript", model.cache_identity()))
    };
    let cache_key = model_id.with("entry-heights");
    let passive_artifacts = super::artifact_card::is_passive(ui.ctx());
    for entry in model.visible_entries() {
        if passive_artifacts {
            break;
        }
        if let TranscriptEntry::Artifacts { presentation } = entry {
            for artifact in &presentation.artifacts {
                super::artifact_card::poll_availability(
                    ui.ctx(),
                    std::path::Path::new(&artifact.path),
                );
            }
        }
    }
    let own_branch = model.last_branch_entry_id();
    if branch.is_some() {
        branch::confirm_modal(ui, branch_action);
    }
    // A body that finishes loading resizes its row. The entry the reader is
    // looking at must not move because of it, even when the pane follows the
    // end of the conversation, so a move caused by that is undone.
    let anchor_key = model_id.with("body-anchor");
    let previous_anchor: Option<ScrollAnchor> = ui.data(|data| data.get_temp(anchor_key));
    let mut first_visible: Option<(usize, f32)> = None;
    // Where the entry the reader was looking at stands now, whether or not it
    // is still the first in view: a row above it that changed height can put
    // another entry first.
    let mut anchored_entry_y: Option<f32> = None;
    let mut corrected_offset: Option<f32> = None;
    let output = egui::ScrollArea::vertical()
        .id_salt(model_id)
        .stick_to_bottom(true)
        .auto_shrink([false, false])
        .show_viewport(ui, |ui, viewport| {
            let used_offset = viewport.min.y;
            // show_viewport supplies content-relative coordinates, whereas
            // cursor positions are in screen coordinates.
            let content_top = ui.min_rect().min.y;
            let screen_viewport = viewport.translate(ui.min_rect().min.to_vec2());
            let viewport = screen_viewport.expand2(egui::vec2(0.0, viewport.height()));
            if let Some(branch) = branch {
                branch::start_versions(ui, model, branch, branch_action);
            }
            let entries = model.visible_entries();
            let mut list =
                virtual_list::VirtualList::load(ui, cache_key, model.visible_entry_ids());
            use std::hash::{Hash, Hasher};
            let mut layout_revision = std::hash::DefaultHasher::new();
            model.cache_identity().hash(&mut layout_revision);
            for row in task_rows {
                (&row.run_id, &row.name, &row.role).hash(&mut layout_revision);
            }
            branch
                .map(|branch| branch.rewind_block)
                .hash(&mut layout_revision);
            chips
                .as_ref()
                .map(|chips| crate::panes::pull_request_chip::revision(chips.by_call))
                .hash(&mut layout_revision);
            let layout_revision = layout_revision.finish();
            let latest_job_entry = latest_shell_job_entries(entries, model.shell_jobs());
            let nesting = crate::panes::transcript_tool::RunJsNesting::new(entries);
            let mut folded_until = 0;
            for (entry_idx, entry) in entries.iter().enumerate() {
                if entry_idx < folded_until || nesting.is_nested(entry_idx) {
                    continue;
                }
                if branch.is_none() && matches!(entry, TranscriptEntry::TurnEnd { .. }) {
                    continue;
                }
                let entry_id = model.visible_entry_id(entry_idx);
                // Determine grouping even for skipped cards: otherwise folded
                // poll entries would reappear as separate rows.
                if matches!(entry, TranscriptEntry::Tool { .. })
                    && !crate::panes::transcript_tool::uses_run_js_layout(entry)
                {
                    folded_until = entry_idx
                        + crate::panes::transcript_tool::card_len(
                            &entries[entry_idx..],
                            model.shell_jobs(),
                        );
                }
                let revision = row_revision(
                    model,
                    (entry_idx..folded_until.max(entry_idx + 1))
                        .chain(nesting.children(entry_idx).iter().copied()),
                    &latest_job_entry,
                    layout_revision,
                );
                let row_top = ui.cursor().top();
                if previous_anchor.is_some_and(|anchor| anchor.entry == entry_id) {
                    anchored_entry_y = Some(row_top - content_top);
                }
                list.row(ui, viewport, entry_id, revision, |ui| {
                    if let TranscriptEntry::TurnEnd { .. } = entry {
                        // Run detail panes show history only; turn actions belong to threads.
                        if let Some(branch) = branch {
                            branch::turn_footer(ui, model, entry_id, branch, branch_action);
                        }
                        return;
                    }
                    if let TranscriptEntry::Branch { kind, .. } = entry {
                        branch::branch_divider(
                            ui,
                            *kind,
                            own_branch == Some(entry_id),
                            branch,
                            branch_action,
                        );
                        return;
                    }
                    if crate::panes::transcript_tool::uses_run_js_layout(entry) {
                        use crate::panes::transcript_tool::{RunJsChild, resolved_shell_job_key};
                        let children: Vec<_> = nesting
                            .children(entry_idx)
                            .iter()
                            .map(|&index| RunJsChild {
                                entry: &entries[index],
                                latest_for_job: resolved_shell_job_key(
                                    &entries[index],
                                    model.shell_jobs(),
                                )
                                .is_some_and(|job| latest_job_entry.get(&job) == Some(&index)),
                            })
                            .collect();
                        crate::panes::transcript_tool::run_js_card(
                            ui,
                            entry,
                            &children,
                            model_id.with(("tool-entry", entry_id)),
                            crate::panes::transcript_tool::ToolCardContext {
                                repo_root,
                                jobs: model.shell_jobs(),
                                latest_for_job: false,
                                agents: task_rows,
                            },
                        );
                        return;
                    }
                    if matches!(entry, TranscriptEntry::Tool { .. }) {
                        use crate::panes::transcript_tool::{
                            resolved_shell_job_key, tool_card_group,
                        };
                        let group = &entries[entry_idx..folded_until];
                        let latest_for_job = group
                            .last()
                            .and_then(|entry| resolved_shell_job_key(entry, model.shell_jobs()))
                            .is_some_and(|job| {
                                latest_job_entry.get(&job) == Some(&(folded_until - 1))
                            });
                        tool_card_group(
                            ui,
                            group,
                            model_id.with(("tool-entry", entry_id)),
                            crate::panes::transcript_tool::ToolCardContext {
                                repo_root,
                                jobs: model.shell_jobs(),
                                latest_for_job,
                                agents: task_rows,
                            },
                        );
                        if let Some(chips) = chips.as_deref_mut() {
                            crate::panes::pull_request_chip::show(ui, chips, group);
                        }
                        return;
                    }
                    ui.add_space(SP_1);
                    if let TranscriptEntry::Artifacts { presentation } = entry {
                        crate::panes::artifact_card::show(
                            ui,
                            pane_id.with(("artifacts", entry_id)),
                            presentation,
                        );
                        return;
                    }
                    if let TranscriptEntry::UserMessage { text, skills } = entry {
                        user_bubble(ui, text, skills);
                        if let Some(branch) = branch {
                            branch::edit_row(ui, model, entry_id, branch, branch_action);
                        }
                        return;
                    }
                    if let TranscriptEntry::AgentMessage {
                        direction,
                        peer_run_id,
                        content,
                        ..
                    } = entry
                    {
                        agent_message::show(ui, *direction, peer_run_id, content, task_rows);
                        return;
                    }
                    if let TranscriptEntry::Unreadable {
                        event_id,
                        run_id,
                        kind,
                        reason,
                        reference,
                    } = entry
                    {
                        unreadable::show(
                            ui,
                            pane_id.with(("unreadable", entry_id)),
                            &unreadable::Gap {
                                event_id: *event_id,
                                run_id: run_id.as_deref(),
                                kind,
                                reason,
                                reference: reference.as_ref(),
                            },
                        );
                        return;
                    }
                    if let Some((icon, color)) = event_icon(entry) {
                        let hover = match entry {
                            TranscriptEntry::Diagnostic { hover, .. } => Some(hover.as_str()),
                            _ => None,
                        };
                        event_line(ui, icon, &entry_label(entry), color, hover);
                        return;
                    }
                    entry_frame(entry).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        match entry {
                            TranscriptEntry::Message { run_id, .. }
                            | TranscriptEntry::Reasoning { run_id, .. } => {
                                if let Some(role) = run_id.as_deref().and_then(|run_id| {
                                    crate::model::tasks::role_for_run(task_rows, run_id)
                                }) {
                                    role_label(ui, role);
                                }
                            }
                            TranscriptEntry::Error { .. }
                            | TranscriptEntry::UserMessage { .. }
                            | TranscriptEntry::Notice { .. }
                            | TranscriptEntry::Diagnostic { .. }
                            | TranscriptEntry::SandboxReview { .. }
                            | TranscriptEntry::Compaction { .. }
                            | TranscriptEntry::CompactionProgress { .. }
                            | TranscriptEntry::Tool { .. }
                            | TranscriptEntry::AgentMessage { .. }
                            | TranscriptEntry::TurnEnd { .. }
                            | TranscriptEntry::Artifacts { .. }
                            | TranscriptEntry::Unreadable { .. }
                            | TranscriptEntry::Branch { .. } => {}
                        }
                        if let TranscriptEntry::Message { text, .. } = entry {
                            crate::body::ui::show_text(
                                ui,
                                pane_id.with(("message-body", entry_id)),
                                text,
                                |ui, text| {
                                    crate::panes::markdown_render::render_markdown_with_base(
                                        ui,
                                        text,
                                        &format!("msg-{entry_id}"),
                                        repo_root,
                                        repo_root,
                                    );
                                },
                            );
                            return;
                        }
                        if let TranscriptEntry::SandboxReview { text, call_id, .. } = entry {
                            sandbox_review::show(ui, text, call_id.as_deref());
                            return;
                        }
                        if let TranscriptEntry::Compaction { .. }
                        | TranscriptEntry::CompactionProgress { .. } = entry
                        {
                            compaction::show(ui, pane_id.with(("compaction", entry_id)), entry);
                            return;
                        }
                        if let TranscriptEntry::Reasoning { text, run_id } = entry {
                            let entry_id = model.visible_entry_id(entry_idx);
                            thinking::show(
                                ui,
                                pane_id.with(("thinking", run_id, entry_id)),
                                (text, model.thinking_is_streaming(entry_id)),
                            );
                            return;
                        }
                        let foreground = match entry {
                            TranscriptEntry::Error { .. }
                            | TranscriptEntry::Diagnostic {
                                severity: event_bus::DiagnosticSeverity::Error,
                                ..
                            } => palette().ERROR_FG,
                            TranscriptEntry::Reasoning { .. } => palette().TEXT_MUTED,
                            _ => palette().TEXT,
                        };
                        let response =
                            ui.label(egui::RichText::new(entry_label(entry)).color(foreground));
                        if let TranscriptEntry::Diagnostic { hover, .. } = entry {
                            response.on_hover_text(hover);
                        }
                    });
                });
                if first_visible.is_none() && ui.cursor().top() > screen_viewport.top() {
                    first_visible = Some((entry_id, row_top - content_top));
                }
            }
            list.store(ui, cache_key);
            corrected_offset = anchor_correction(
                ui,
                previous_anchor,
                anchored_entry_y,
                first_visible,
                (used_offset, screen_viewport.height()),
            );
            if let Some(identity) = identity {
                crate::panes::ledger::ledger_section(ui, identity.run_id, identity.ledger);
            }
            requests.and_then(|requests| requests.show(ui))
        });
    virtual_list::record_layout(ui, output.content_size, output.state.offset);
    if let Some((entry, content_y)) = first_visible {
        let offset_y = corrected_offset.unwrap_or(output.state.offset.y);
        ui.data_mut(|data| {
            data.insert_temp(
                anchor_key,
                ScrollAnchor {
                    entry,
                    content_y,
                    offset_y,
                },
            );
            data.insert_temp(
                egui::Id::new(LATEST_ANCHOR_ID),
                (entry, content_y - offset_y),
            );
        });
    }
    output.inner
}

const LATEST_ANCHOR_ID: &str = "transcript-latest-anchor";

/// The entry the reader is looking at (the first in view) and where it is on
/// the pane, as the last drawn transcript saw it. For tests of the position a
/// loading body keeps.
#[doc(hidden)]
#[must_use]
pub fn first_visible_entry_for_tests(ctx: &egui::Context) -> Option<(usize, f32)> {
    ctx.data(|data| data.get_temp(egui::Id::new(LATEST_ANCHOR_ID)))
}

/// The first entry in view, and where it was: its place in the content and
/// the scroll offset that put it on screen.
#[derive(Clone, Copy)]
struct ScrollAnchor {
    entry: usize,
    content_y: f32,
    offset_y: f32,
}

/// If a body changed between waiting and shown this pass and moved the entry
/// the reader was looking at (while they did not scroll), scrolls so that it
/// stays where it was on screen, in the same pass. Returns the offset it
/// scrolled to.
fn anchor_correction(
    ui: &egui::Ui,
    previous: Option<ScrollAnchor>,
    anchored_entry_y: Option<f32>,
    first_visible: Option<(usize, f32)>,
    (used_offset, viewport_height): (f32, f32),
) -> Option<f32> {
    let transitioned = crate::body::ui::take_transition(ui.ctx());
    let (entry, content_y) = match (previous, anchored_entry_y) {
        (Some(previous), Some(y)) => (previous.entry, y),
        _ => first_visible?,
    };
    let previous = previous.filter(|previous| previous.entry == entry)?;
    let scrolled = ui.input(|input| input.is_scrolling() || input.pointer.any_down());
    if !transitioned || scrolled {
        return None;
    }
    let wanted = content_y - (previous.content_y - previous.offset_y);
    // A pane that follows the end of the conversation is pulled down by the
    // growth once the rows are laid out; that moves the entry just as a change
    // above it does.
    let followed_end = (ui.min_rect().height() - viewport_height).max(0.0);
    if (wanted - used_offset).abs() <= 0.5 && (followed_end - wanted).abs() <= 0.5 {
        return None;
    }
    // Any explicit scroll adjustment also stops the pane from following the end.
    let mut delta = used_offset - wanted;
    if delta.abs() < 1e-3 {
        delta = -1e-3;
    }
    ui.scroll_with_delta_animation(egui::vec2(0.0, delta), egui::style::ScrollAnimation::none());
    Some(wanted)
}

/// Single-line lifecycle entries render as a muted icon line, not a card.
fn event_icon(entry: &TranscriptEntry) -> Option<(&'static str, Color32)> {
    match entry {
        TranscriptEntry::Notice { .. }
        | TranscriptEntry::Diagnostic {
            severity: event_bus::DiagnosticSeverity::Info | event_bus::DiagnosticSeverity::Warning,
            ..
        } => Some((icons::INFO, palette().TEXT_MUTED)),
        TranscriptEntry::Diagnostic {
            severity: event_bus::DiagnosticSeverity::Error,
            ..
        }
        | TranscriptEntry::AgentMessage { .. }
        | TranscriptEntry::Error { .. }
        | TranscriptEntry::UserMessage { .. }
        | TranscriptEntry::SandboxReview { .. }
        | TranscriptEntry::Compaction { .. }
        | TranscriptEntry::CompactionProgress { .. }
        | TranscriptEntry::Message { .. }
        | TranscriptEntry::Reasoning { .. }
        | TranscriptEntry::Tool { .. }
        | TranscriptEntry::TurnEnd { .. }
        | TranscriptEntry::Artifacts { .. }
        | TranscriptEntry::Unreadable { .. }
        | TranscriptEntry::Branch { .. } => None,
    }
}

/// Assistant text sits directly on the canvas; only entries that need to
/// stand out (errors, reviews, compaction) get a borderless tinted surface.
fn entry_frame(entry: &TranscriptEntry) -> egui::Frame {
    let fill = match entry {
        TranscriptEntry::Error { .. }
        | TranscriptEntry::Diagnostic {
            severity: event_bus::DiagnosticSeverity::Error,
            ..
        } => palette().ERROR_SURFACE,
        TranscriptEntry::SandboxReview { .. } => palette().WARNING_SURFACE,
        TranscriptEntry::Compaction { .. } | TranscriptEntry::CompactionProgress { .. } => {
            palette().SURFACE
        }
        _ => {
            return egui::Frame::new()
                .inner_margin(egui::Margin::symmetric(SP_1 as i8, SP_1 as i8));
        }
    };
    soft_frame(fill)
}

fn user_bubble(ui: &mut egui::Ui, text: &str, skills: &[String]) {
    ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
        let max_width = (ui.available_width() * 0.8).max(120.0);
        soft_frame(palette().SURFACE_RAISED)
            .corner_radius(egui::CornerRadius::same(R_XL))
            .show(ui, |ui| {
                ui.set_max_width(max_width);
                ui.with_layout(egui::Layout::top_down(egui::Align::LEFT), |ui| {
                    let response = ui.add(
                        egui::Label::new(egui::RichText::new(text).color(palette().TEXT)).wrap(),
                    );
                    let label = format!("You: {text}");
                    response.widget_info(|| {
                        egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &label)
                    });
                    if !skills.is_empty() {
                        ui.horizontal_wrapped(|ui| {
                            for skill in skills {
                                ui.label(
                                    egui::RichText::new(icons::with_icon(
                                        icons::SPARKLE,
                                        format!("skill: {skill}"),
                                    ))
                                    .small()
                                    .color(palette().TEXT_MUTED),
                                )
                                .on_hover_text("Skill body attached when sent");
                            }
                        });
                    }
                });
            });
    });
}

fn event_line(ui: &mut egui::Ui, icon: &str, text: &str, color: Color32, hover: Option<&str>) {
    ui.horizontal_wrapped(|ui| {
        ui.add_space(SP_1);
        ui.spacing_mut().item_spacing.x = SP_2;
        ui.label(
            egui::RichText::new(icon)
                .size(FONT_SMALL + 1.0)
                .color(color),
        );
        let response = ui.add(
            egui::Label::new(
                egui::RichText::new(text)
                    .size(FONT_SMALL)
                    .color(palette().TEXT_MUTED),
            )
            .wrap(),
        );
        response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, text));
        if let Some(hover) = hover {
            response.on_hover_text(hover);
        }
    });
}

fn role_label(ui: &mut egui::Ui, role: &str) {
    let response = ui.label(
        medium(with_icon(icons::ROBOT, role))
            .size(FONT_SMALL)
            .color(palette().TEXT_MUTED),
    );
    let label = format!("[{role}]");
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &label));
}

fn entry_label(entry: &TranscriptEntry) -> String {
    match entry {
        TranscriptEntry::UserMessage { text, .. } => format!("You: {text}"),
        TranscriptEntry::Notice { text }
        | TranscriptEntry::Diagnostic { text, .. }
        | TranscriptEntry::SandboxReview { text, .. }
        | TranscriptEntry::Error { text } => text.clone(),
        // Bodies are drawn through `show_text`; this label never sees a deferred one.
        TranscriptEntry::Message { text, .. } => format!("Message: {}", text.inline_or_empty()),
        TranscriptEntry::Reasoning { text, .. } => text.inline_or_empty().to_owned(),
        TranscriptEntry::Unreadable { event_id, .. } => {
            format!("Event {event_id} could not be read")
        }
        TranscriptEntry::Compaction { .. } | TranscriptEntry::CompactionProgress { .. } => {
            compaction::summary(entry).unwrap_or_default()
        }
        TranscriptEntry::Tool {
            tool_name,
            call_id,
            status,
            ..
        } => format!("Tool {tool_name} ({call_id}): {status:?}"),
        TranscriptEntry::AgentMessage {
            direction,
            peer_run_id,
            content,
            ..
        } => agent_message::label(*direction, peer_run_id, content, &[]),
        TranscriptEntry::TurnEnd {
            run_id,
            context_len,
        } => {
            format!("Turn completed ({run_id} @ {context_len})")
        }
        TranscriptEntry::Branch {
            source_thread_id, ..
        } => {
            format!("Branched from {source_thread_id}")
        }
        TranscriptEntry::Artifacts { presentation } => {
            crate::panes::artifact_card::summary(presentation)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::{Harness, kittest::Queryable};

    #[test]
    fn ledger_section_renders_entries_for_selected_run() {
        // Given: a selected run with a transcript and ordered ledger entries.
        let mut model = TranscriptModel::default();
        model.push(TranscriptEntry::Message {
            text: "Transcript content".into(),
            run_id: None,
        });
        let entries = [
            storage::RunLedgerEntry {
                seq: 1,
                run_id: "selected".into(),
                body: "First entry".into(),
                created_at_ns: 0,
            },
            storage::RunLedgerEntry {
                seq: 3,
                run_id: "selected".into(),
                body: "Last entry".into(),
                created_at_ns: 0,
            },
        ];
        let mut harness = Harness::builder()
            .with_size(egui::vec2(500.0, 300.0))
            .build_ui(|ui| {
                crate::theme::install(ui.ctx());
                run_detail_body(
                    ui,
                    &model,
                    (
                        Some(AgentIdentity {
                            run_id: "selected",
                            name: None,
                            role: None,
                            ledger: &entries,
                        }),
                        &[],
                    ),
                    None,
                    None,
                    (None, &mut None),
                    None,
                );
            });
        harness.run_steps(2);
        assert!(
            harness
                .query_by_label("Current revision 3: Last entry")
                .is_none()
        );
        // When: the current note is expanded, previous revisions remain separate.
        harness.get_by_label("> Working note").click();
        harness.run_steps(2);
        let last = harness
            .get_by_label("Current revision 3: Last entry")
            .rect();
        assert!(last.top() > harness.get_by_label("Transcript content").rect().bottom());
        assert!(harness.query_by_label("Revision 1: First entry").is_none());
        harness.get_by_label("Previous revisions").click();
        harness.run_steps(2);
        assert!(harness.get_by_label("Revision 1: First entry").rect().top() > last.bottom());
        if let Some(directory) = std::env::var_os("LEDGER_EVIDENCE_DIR") {
            harness
                .render()
                .unwrap()
                .save(std::path::PathBuf::from(directory).join("ledger-expanded.png"))
                .unwrap();
        }
    }

    #[test]
    fn ledger_section_hidden_when_empty() {
        // Given: a selected run without ledger entries.
        let model = TranscriptModel::default();
        // When: the run detail surface is rendered.
        let harness = Harness::builder().build_ui(|ui| {
            run_detail_body(
                ui,
                &model,
                (
                    Some(AgentIdentity {
                        run_id: "empty",
                        name: None,
                        role: None,
                        ledger: &[],
                    }),
                    &[],
                ),
                None,
                None,
                (None, &mut None),
                None,
            );
        });
        // Then: no ledger header is exposed.
        assert!(harness.query_by_label("> Working note").is_none());
    }

    #[test]
    fn thread_header_omits_phase_pill() {
        for (phase, label) in [
            (ThreadRunPhase::Running, "Running"),
            (ThreadRunPhase::Waiting, "Waiting for input"),
            (ThreadRunPhase::Done, "Done"),
            (ThreadRunPhase::Stopped, "Stopped"),
        ] {
            let mut harness = Harness::builder()
                .with_size(egui::vec2(400.0, 80.0))
                .build_ui(move |ui| {
                    crate::theme::install(ui.ctx());
                    let ctx = ConversationContext {
                        todo: None,
                        goal: None,
                        requests: None,
                        task_rows: &[],
                        phase_unread: true,
                        has_project: true,
                        active_thread_title: Some("Chat"),
                        extra_read_dirs: &[],
                        parent_thread: None,
                        child_threads: Vec::new(),
                        lane: None,
                        lead: None,
                        thread_metrics: None,
                        phase: Some(phase),
                        sandbox_picker: Default::default(),
                        model_picker: crate::panes::model_picker::ModelPickerContext {
                            profiles: &[],
                            preference: None,
                            default_model: None,
                            enabled: false,
                        },
                        branch: None,
                        pull_request: None,
                        pull_request_chips: None,
                    };
                    header_strip(ui, &None, &ctx, &mut None);
                });
            harness.run_steps(2);
            assert!(harness.query_by_label(label).is_none());
            assert!(harness.get_by_label("Thread: Chat").rect().right() < 400.0);
        }
    }

    fn ready_lane_detail() -> crate::model::lanes::LaneDetail {
        use event_bus::lane::{
            LaneBrief, LanePolicy, LaneReport, LaneReportProducer, LaneScope, LaneStatus,
            LaneWorkspaceDeclaration,
        };

        let record = storage::lanes::LaneRecord {
            lane_id: "lane-1".into(),
            thread_id: "lane-thread".into(),
            origin_thread_id: "lead".into(),
            origin_run_id: "lead-run".into(),
            creation_key: "call".into(),
            scope: LaneScope::Lane,
            lead_thread_id: Some("lead".into()),
            policy: LanePolicy::Attached,
            management_revision: 1,
            brief: LaneBrief {
                objective: "Ship it".into(),
                inputs: vec![],
                deliverables: vec![],
                acceptance_criteria: vec![],
                constraints: vec![],
                workspace: LaneWorkspaceDeclaration::default(),
                report_policy: LanePolicy::Attached,
            },
            brief_revision: 1,
            start_seed: String::new(),
            start_blocked_reason: None,
            root_run_id: None,
            generation: 1,
            latest_report_seq: 1,
            created_at_ns: 0,
            updated_at_ns: 0,
            last_activity_at_ns: 0,
        };
        crate::model::lanes::LaneDetail {
            record,
            latest_report: Some(LaneReport {
                artifact_revision: None,
                failure_episode: None,
                lane_id: "lane-1".into(),
                report_seq: 1,
                status: LaneStatus::Ready,
                summary: "all done".into(),
                artifact_refs: vec![],
                needs_human: false,
                question_refs: vec![],
                reason_code: None,
                producer: LaneReportProducer::LaneAgent,
                management_revision: 1,
                brief_revision: 1,
                created_at_ns: 0,
            }),
            delivery: Default::default(),
        }
    }

    #[test]
    fn lane_header_words_a_ready_report_as_unverified() {
        use event_bus::lane::LanePolicy;

        let detail = ready_lane_detail();
        let mut harness = Harness::builder()
            .with_size(egui::vec2(480.0, 200.0))
            .build_ui(move |ui| {
                crate::theme::install(ui.ctx());
                let ctx = ConversationContext {
                    todo: None,
                    goal: None,
                    requests: None,
                    task_rows: &[],
                    phase_unread: false,
                    has_project: true,
                    active_thread_title: Some("Lane"),
                    extra_read_dirs: &[],
                    parent_thread: None,
                    child_threads: Vec::new(),
                    lane: Some(LaneContext {
                        detail: &detail,
                        origin: None,
                        delivery_bound: true,
                    }),
                    lead: None,
                    thread_metrics: None,
                    phase: None,
                    sandbox_picker: Default::default(),
                    model_picker: crate::panes::model_picker::ModelPickerContext {
                        profiles: &[],
                        preference: None,
                        default_model: None,
                        enabled: false,
                    },
                    branch: None,
                    pull_request: None,
                    pull_request_chips: None,
                };
                header_strip(ui, &None, &ctx, &mut None);
            });
        harness.run_steps(2);

        assert!(harness.query_by_label("ready (unverified)").is_some());
        assert!(harness.query_by_label("lane · attached").is_some());
        assert!(
            harness
                .query_by_label(crate::model::lanes::routing_notice(
                    LanePolicy::Attached,
                    true
                ))
                .is_some()
        );
        assert!(harness.query_by_label("Report #1: all done").is_some());
    }

    type Captured = std::rc::Rc<std::cell::RefCell<Option<AgentPaneAction>>>;

    /// Draws a thread header for a lane and/or a lead, collecting the action a click raises.
    fn delivery_header(
        lane: Option<crate::model::lanes::LaneDetail>,
        lead: Option<(Vec<crate::model::lanes::LaneDetail>, usize)>,
        bound: bool,
    ) -> (Harness<'static>, Captured) {
        let captured: Captured = Default::default();
        let sink = captured.clone();
        let harness = Harness::builder()
            .with_size(egui::vec2(640.0, 400.0))
            .build_ui(move |ui| {
                crate::theme::install(ui.ctx());
                let ctx = ConversationContext {
                    todo: None,
                    goal: None,
                    requests: None,
                    task_rows: &[],
                    phase_unread: false,
                    has_project: true,
                    active_thread_title: Some("Thread"),
                    extra_read_dirs: &[],
                    parent_thread: None,
                    child_threads: Vec::new(),
                    lane: lane.as_ref().map(|detail| LaneContext {
                        detail,
                        origin: None,
                        delivery_bound: bound,
                    }),
                    lead: lead.as_ref().map(|(lanes, waiting)| LeadContext {
                        thread_id: "lead",
                        lanes: lanes.iter().collect(),
                        waiting_questions: *waiting,
                    }),
                    thread_metrics: None,
                    phase: None,
                    sandbox_picker: Default::default(),
                    model_picker: crate::panes::model_picker::ModelPickerContext {
                        profiles: &[],
                        preference: None,
                        default_model: None,
                        enabled: false,
                    },
                    branch: None,
                    pull_request: None,
                    pull_request_chips: None,
                };
                let mut action = None;
                header_strip(ui, &None, &ctx, &mut action);
                if action.is_some() {
                    *sink.borrow_mut() = action;
                }
            });
        (harness, captured)
    }

    fn blocked() -> storage::lane_delivery::QuestionCapacityBlocked {
        storage::lane_delivery::QuestionCapacityBlocked {
            requester_run_id: "run-1".into(),
            capacity: storage::lane_delivery::QuestionCapacity::Requester,
            attempts: 3,
            first_at_ns: 1,
            last_at_ns: 2,
        }
    }

    #[test]
    fn the_lane_card_words_routing_by_policy_and_by_whether_delivery_runs() {
        use event_bus::lane::LanePolicy;
        for (policy, bound) in [
            (LanePolicy::Attached, true),
            (LanePolicy::Summary, true),
            (LanePolicy::Detached, true),
            (LanePolicy::Attached, false),
        ] {
            let mut detail = ready_lane_detail();
            detail.record.policy = policy;
            let (mut harness, _) = delivery_header(Some(detail), None, bound);
            harness.run_steps(2);
            assert!(
                harness
                    .query_by_label(crate::model::lanes::routing_notice(policy, bound))
                    .is_some(),
                "{policy:?} bound={bound}"
            );
        }
    }

    #[test]
    fn the_lane_card_shows_a_capacity_diagnostic_and_offers_no_retry() {
        let mut detail = ready_lane_detail();
        detail.delivery.capacity_blocked = Some(blocked());
        let (mut harness, captured) = delivery_header(Some(detail), None, true);
        harness.run_steps(2);

        let line = crate::model::lanes::LaneDeliveryView {
            capacity_blocked: Some(blocked()),
            ..Default::default()
        }
        .capacity_line()
        .unwrap();
        assert!(harness.query_by_label(&line).is_some());
        assert!(harness.query_by_label("Resume notifications").is_none());
        assert!(captured.borrow().is_none());
    }

    #[test]
    fn a_paused_wake_shows_why_and_its_resume_button_resumes_the_lead() {
        let mut detail = ready_lane_detail();
        detail.delivery.guard.suppressed = true;
        detail.delivery.guard.no_progress_batches = 20;
        let (mut harness, captured) = delivery_header(Some(detail.clone()), None, true);
        harness.run_steps(2);

        assert!(
            harness
                .query_by_label(&detail.delivery.suppressed_line().unwrap())
                .is_some()
        );
        harness.get_by_label("Resume notifications").click();
        harness.run_steps(2);
        assert_eq!(
            captured.borrow().as_ref().map(|action| match action {
                AgentPaneAction::Lane(action) => action.clone(),
                _ => panic!("another action"),
            }),
            Some(LaneAction::ResumeDelivery {
                lead: "lead".into()
            })
        );
    }

    #[test]
    fn a_stopped_lead_is_continued_by_the_users_own_path_not_by_progress() {
        let mut detail = ready_lane_detail();
        detail.delivery.lead_stopped = true;
        let (mut harness, captured) = delivery_header(Some(detail), None, true);
        harness.run_steps(2);

        assert!(harness.query_by_label("Resume notifications").is_none());
        harness.get_by_label("Continue lead").click();
        harness.run_steps(2);
        assert!(matches!(
            captured.borrow().as_ref(),
            Some(AgentPaneAction::Lane(LaneAction::ContinueLead { lead })) if lead == "lead"
        ));
    }

    #[test]
    fn a_lead_sees_how_many_lane_questions_wait_for_it_and_nothing_when_all_is_quiet() {
        let (mut harness, _) = delivery_header(None, Some((vec![ready_lane_detail()], 2)), true);
        harness.run_steps(2);
        assert!(
            harness
                .query_by_label_contains("2 lane question(s) wait for your answer here")
                .is_some()
        );

        let (mut quiet, _) = delivery_header(None, Some((vec![ready_lane_detail()], 0)), true);
        quiet.run_steps(2);
        assert!(quiet.query_by_label_contains("lane question(s)").is_none());
        assert!(quiet.query_by_label_contains("Wake paused").is_none());
    }

    #[test]
    fn a_lead_view_lists_the_paused_and_blocked_lanes_with_the_resume_operation() {
        let mut paused = ready_lane_detail();
        paused.delivery.guard.suppressed = true;
        paused.delivery.guard.no_progress_batches = 20;
        let mut full = ready_lane_detail();
        full.record.lane_id = "lane-2".into();
        full.delivery.capacity_blocked = Some(blocked());
        let (mut harness, captured) = delivery_header(None, Some((vec![paused, full], 0)), true);
        harness.run_steps(2);

        assert!(
            harness
                .query_by_label_contains("Question capacity reached")
                .is_some()
        );
        assert!(harness.query_by_label("Lane lane-2").is_some());
        harness.get_by_label("Resume notifications").click();
        harness.run_steps(2);
        assert!(matches!(
            captured.borrow().as_ref(),
            Some(AgentPaneAction::Lane(LaneAction::ResumeDelivery { lead })) if lead == "lead"
        ));
    }

    #[test]
    fn a_stopped_lead_offers_continue_never_a_resume_that_would_not_restart_it() {
        let mut detail = ready_lane_detail();
        detail.delivery.guard.suppressed = true;
        detail.delivery.guard.no_progress_batches = 20;
        detail.delivery.lead_stopped = true;

        let (mut lane_card, _) = delivery_header(Some(detail.clone()), None, true);
        lane_card.run_steps(2);
        assert!(lane_card.query_by_label("Resume notifications").is_none());
        assert!(lane_card.query_by_label("Continue lead").is_some());
        assert!(lane_card.query_by_label_contains("Wake paused").is_some());

        let (mut lead_view, _) = delivery_header(None, Some((vec![detail], 0)), true);
        lead_view.run_steps(2);
        assert!(lead_view.query_by_label("Resume notifications").is_none());
        assert!(lead_view.query_by_label("Continue lead").is_some());
        assert!(lead_view.query_by_label_contains("Wake paused").is_some());
    }
}
