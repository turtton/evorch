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
use header::header_strip;
mod queue;
mod sandbox_review;
mod thinking;
mod virtual_list;
#[doc(hidden)]
pub use virtual_list::{set_transcript_virtualization_for_tests, transcript_layout_for_tests};

#[derive(Debug, Clone, Copy)]
pub struct AgentIdentity<'a> {
    pub run_id: &'a str,
    pub name: Option<&'a str>,
    pub role: Option<&'a str>,
    pub ledger: &'a [storage::RunLedgerEntry],
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
    pub thread_metrics: Option<crate::model::telemetry::ThreadMetrics>,
    pub phase: Option<ThreadRunPhase>,
    pub next_thread_title: String,
    pub model_picker: crate::panes::model_picker::ModelPickerContext<'a>,
    pub sandbox_picker: crate::panes::composer::SandboxPickerContext,
    /// Present only while showing a thread conversation.
    pub branch: Option<BranchContext<'a>>,
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
        composer_panel::composer_panel(
            ui,
            &ctx,
            composer,
            picker_state,
            identity.is_some(),
            &mut action,
        );
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
                    if let Some(request) = run_detail_body(
                        ui,
                        model,
                        (identity, ctx.task_rows),
                        repo_root,
                        ctx.requests.as_mut(),
                        (ctx.branch.as_ref(), &mut branch_action),
                    ) {
                        action = Some(AgentPaneAction::Request(request));
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
            *action = Some(AgentPaneAction::Sidebar(SidebarAction::CreateThread(
                ctx.next_thread_title.clone(),
            )));
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
    );
}

fn run_detail_body(
    ui: &mut egui::Ui,
    model: &TranscriptModel,
    context: (Option<AgentIdentity<'_>>, &[crate::model::tasks::TaskRow]),
    repo_root: Option<&std::path::Path>,
    requests: Option<&mut super::requests::ConversationRequests<'_>>,
    (branch, branch_action): (Option<&BranchContext<'_>>, &mut Option<SidebarAction>),
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
    let own_branch = model.last_branch_entry_id();
    if branch.is_some() {
        branch::confirm_modal(ui, branch_action);
    }
    let output = egui::ScrollArea::vertical()
        .id_salt(model_id)
        .stick_to_bottom(true)
        .auto_shrink([false, false])
        .show_viewport(ui, |ui, viewport| {
            // show_viewport supplies content-relative coordinates, whereas
            // cursor positions are in screen coordinates.
            let viewport = viewport
                .translate(ui.min_rect().min.to_vec2())
                .expand2(egui::vec2(0.0, viewport.height()));
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
                            pane_id.with(("tool-entry", entry_id)),
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
                            pane_id.with(("tool-entry", entry_id)),
                            crate::panes::transcript_tool::ToolCardContext {
                                repo_root,
                                jobs: model.shell_jobs(),
                                latest_for_job,
                                agents: task_rows,
                            },
                        );
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
                    if let TranscriptEntry::UserMessage { text } = entry {
                        user_bubble(ui, text);
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
                            | TranscriptEntry::Tool { .. }
                            | TranscriptEntry::AgentMessage { .. }
                            | TranscriptEntry::TurnEnd { .. }
                            | TranscriptEntry::Artifacts { .. }
                            | TranscriptEntry::Branch { .. } => {}
                        }
                        if let TranscriptEntry::Message { text, .. } = entry {
                            crate::panes::markdown_render::render_markdown_with_base(
                                ui,
                                text,
                                &format!("msg-{entry_id}"),
                                repo_root,
                                repo_root,
                            );
                            return;
                        }
                        if let TranscriptEntry::SandboxReview { text, call_id, .. } = entry {
                            sandbox_review::show(ui, text, call_id.as_deref());
                            return;
                        }
                        if let TranscriptEntry::Compaction { .. } = entry {
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
            }
            list.store(ui, cache_key);
            if let Some(identity) = identity {
                crate::panes::ledger::ledger_section(ui, identity.run_id, identity.ledger);
            }
            requests.and_then(|requests| requests.show(ui))
        });
    virtual_list::record_layout(ui, output.content_size, output.state.offset);
    output.inner
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
        | TranscriptEntry::Message { .. }
        | TranscriptEntry::Reasoning { .. }
        | TranscriptEntry::Tool { .. }
        | TranscriptEntry::TurnEnd { .. }
        | TranscriptEntry::Artifacts { .. }
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
        TranscriptEntry::Compaction { .. } => palette().SURFACE,
        _ => {
            return egui::Frame::new()
                .inner_margin(egui::Margin::symmetric(SP_1 as i8, SP_1 as i8));
        }
    };
    soft_frame(fill)
}

fn user_bubble(ui: &mut egui::Ui, text: &str) {
    let (text, skills) = crate::model::composer::split_skill_attachments(text);
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
        TranscriptEntry::UserMessage { text } => format!(
            "You: {}",
            crate::model::composer::split_skill_attachments(text).0
        ),
        TranscriptEntry::Notice { text }
        | TranscriptEntry::Diagnostic { text, .. }
        | TranscriptEntry::SandboxReview { text, .. }
        | TranscriptEntry::Error { text } => text.clone(),
        TranscriptEntry::Message { text, .. } => format!("Message: {text}"),
        TranscriptEntry::Reasoning { text, .. } => text.clone(),
        TranscriptEntry::Compaction { .. } => compaction::summary(entry).unwrap_or_default(),
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
                        thread_metrics: None,
                        phase: Some(phase),
                        next_thread_title: String::new(),
                        sandbox_picker: Default::default(),
                        model_picker: crate::panes::model_picker::ModelPickerContext {
                            profiles: &[],
                            preference: None,
                            default_model: None,
                            enabled: false,
                        },
                        branch: None,
                    };
                    header_strip(ui, &None, &ctx, &mut None);
                });
            harness.run_steps(2);
            assert!(harness.query_by_label(label).is_none());
            assert!(harness.get_by_label("Thread: Chat").rect().right() < 400.0);
        }
    }
}
