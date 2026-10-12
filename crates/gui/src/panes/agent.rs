//! Agent トランスクリプトペインの描画。

use egui::Color32;
use workspace_ui::ThreadRunPhase;

use crate::model::composer::ComposerModel;
use crate::model::transcript::{MessageDirection, TranscriptEntry, TranscriptModel};
use crate::panes::agents::AgentsAction;
use crate::panes::composer::{ComposerAction, composer_strip};
use crate::panes::sidebar::SidebarAction;
use crate::theme::icons::{self, with_icon};
use crate::theme::text::medium;
use crate::theme::tokens::*;
use crate::theme::widgets::{empty_state, pane_root, soft_frame};

mod goal;
pub use goal::GoalAction;
mod branch;
pub use branch::BranchContext;
mod header;
use header::{header_strip, status_strip};
mod sandbox_review;
mod thinking;

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
        let composer_id = ui.id().with("composer-height");
        let previous_height = ui
            .data(|data| data.get_temp::<f32>(composer_id))
            .unwrap_or(COMPOSER_MIN_HEIGHT);
        let lines = composer.input.split('\n').count().max(1);
        let line_height = ui.text_style_height(&egui::TextStyle::Body);
        let input_height = (lines as f32 * line_height + 2.0 * SP_2)
            .clamp(COMPOSER_MIN_HEIGHT - 2.0 * SP_2, COMPOSER_MAX_HEIGHT);
        let extra = input_height - (COMPOSER_MIN_HEIGHT - 2.0 * SP_2);
        let previous_extra = ui.data(|data| {
            data.get_temp::<f32>(composer_id.with("lines"))
                .unwrap_or(0.0)
        });
        // Carry forward the measured chrome/attachments, but replace the old
        // input height so deleting lines shrinks the composer too.
        let composer_height = (previous_height - previous_extra + extra)
            .max(COMPOSER_MIN_HEIGHT)
            .min((ui.available_height() - ROW_COMPACT).max(COMPOSER_MIN_HEIGHT));
        ui.data_mut(|data| data.insert_temp(composer_id.with("lines"), extra));
        egui::Panel::bottom(ui.id().with("composer"))
            .exact_size(composer_height)
            .resizable(false)
            .show_separator_line(false)
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
                let strip = ui.scope(|ui| {
                    let vertical_spacing = ui.spacing().item_spacing.y;
                    if ctx.goal.is_some() || ctx.todo.is_some() {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        if let Some(goal_action) = goal::progress_strip(ui, ctx.goal, ctx.todo) {
                            action = Some(AgentPaneAction::Goal(goal_action));
                        }
                    }
                    let result = ui
                        .push_id("composer-strip", |ui| {
                            composer_strip(
                                ui,
                                composer,
                                ctx.model_picker,
                                picker_state,
                                ctx.phase,
                                ctx.sandbox_picker,
                            )
                        })
                        .inner;
                    ui.spacing_mut().item_spacing.y = vertical_spacing;
                    status_strip(ui, &ctx);
                    result
                });
                let height = strip.response.rect.height();
                ui.data_mut(|data| data.insert_temp(composer_id, height));
                if (height - composer_height).abs() > 0.5 {
                    ui.ctx().request_repaint();
                }
                if let Some(composer_action) = strip.inner {
                    action = Some(match composer_action {
                        ComposerAction::ModelPreference(preference) => {
                            AgentPaneAction::ModelPreference(preference)
                        }
                        other => AgentPaneAction::Composer(other),
                    });
                }
            });
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

pub fn transcript_body(ui: &mut egui::Ui, model: &TranscriptModel) {
    transcript_body_with_repo_root(ui, model, None);
}

pub fn transcript_body_with_repo_root(
    ui: &mut egui::Ui,
    model: &TranscriptModel,
    repo_root: Option<&std::path::Path>,
) {
    run_detail_body(ui, model, (None, &[]), repo_root, None, (None, &mut None));
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
    let own_branch = model.last_branch_entry_id();
    if branch.is_some() {
        branch::confirm_modal(ui, branch_action);
    }
    egui::ScrollArea::vertical()
        .stick_to_bottom(true)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if let Some(branch) = branch {
                branch::start_versions(ui, branch, branch_action);
            }
            let entries = model.visible_entries();
            let latest_job_entry = latest_shell_job_entries(entries, model.shell_jobs());
            let mut folded_until = 0;
            for (entry_idx, entry) in entries.iter().enumerate() {
                if entry_idx < folded_until {
                    continue;
                }
                let entry_id = model.visible_entry_id(entry_idx);
                if let TranscriptEntry::TurnEnd { .. } = entry {
                    // Run detail panes show history only; turn actions belong to threads.
                    if let Some(branch) = branch {
                        branch::turn_footer(ui, model, entry_id, branch, branch_action);
                    }
                    continue;
                }
                if let TranscriptEntry::Branch { kind, .. } = entry {
                    branch::branch_divider(
                        ui,
                        *kind,
                        own_branch == Some(entry_id),
                        branch,
                        branch_action,
                    );
                    continue;
                }
                if matches!(entry, TranscriptEntry::Tool { .. }) {
                    use crate::panes::transcript_tool::{
                        card_len, resolved_shell_job_key, tool_card_group,
                    };
                    folded_until = entry_idx + card_len(&entries[entry_idx..], model.shell_jobs());
                    let group = &entries[entry_idx..folded_until];
                    let latest_for_job = group
                        .last()
                        .and_then(|entry| resolved_shell_job_key(entry, model.shell_jobs()))
                        .is_some_and(|job| latest_job_entry.get(&job) == Some(&(folded_until - 1)));
                    tool_card_group(
                        ui,
                        group,
                        pane_id.with(("tool-entry", entry_id)),
                        crate::panes::transcript_tool::ToolCardContext {
                            repo_root,
                            jobs: model.shell_jobs(),
                            latest_for_job,
                        },
                    );
                    continue;
                }
                ui.add_space(SP_1);
                if let TranscriptEntry::Artifacts { presentation } = entry {
                    crate::panes::artifact_card::show(
                        ui,
                        pane_id.with(("artifacts", entry_id)),
                        presentation,
                    );
                    continue;
                }
                if let TranscriptEntry::UserMessage { text } = entry {
                    user_bubble(ui, text);
                    if let Some(branch) = branch {
                        branch::edit_button(ui, model, entry_id, branch);
                    }
                    continue;
                }
                if let Some((icon, color)) = event_icon(entry) {
                    event_line(ui, icon, &entry_label(entry), color);
                    continue;
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
                            &format!("msg-{entry_idx}"),
                            repo_root,
                        );
                        return;
                    }
                    if let TranscriptEntry::SandboxReview {
                        text,
                        run_id,
                        call_id,
                    } = entry
                    {
                        sandbox_review::show(
                            ui,
                            pane_id.with(("sandbox-review", model.visible_entry_id(entry_idx))),
                            text,
                            run_id.as_deref(),
                            call_id.as_deref(),
                        );
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
                        TranscriptEntry::Error { .. } => palette().ERROR_FG,
                        TranscriptEntry::Reasoning { .. } => palette().TEXT_MUTED,
                        _ => palette().TEXT,
                    };
                    ui.label(egui::RichText::new(entry_label(entry)).color(foreground));
                });
            }
            if let Some(identity) = identity {
                crate::panes::ledger::ledger_section(ui, identity.run_id, identity.ledger);
            }
            requests.and_then(|requests| requests.show(ui))
        })
        .inner
}

/// Single-line lifecycle entries render as a muted icon line, not a card.
fn event_icon(entry: &TranscriptEntry) -> Option<(&'static str, Color32)> {
    match entry {
        TranscriptEntry::Notice { .. } => Some((icons::INFO, palette().TEXT_MUTED)),
        TranscriptEntry::AgentMessage { direction, .. } => Some(match direction {
            MessageDirection::Incoming => (icons::ARROW_BEND_DOWN_RIGHT, palette().SUCCESS),
            MessageDirection::Outgoing => (icons::PAPER_PLANE_RIGHT, palette().WARNING_FG),
        }),
        TranscriptEntry::Error { .. }
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
        TranscriptEntry::Error { .. } => palette().ERROR_SURFACE,
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
                                .on_hover_text("送信時に skill 本文を添付済み");
                            }
                        });
                    }
                });
            });
    });
}

fn event_line(ui: &mut egui::Ui, icon: &str, text: &str, color: Color32) {
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
        | TranscriptEntry::SandboxReview { text, .. }
        | TranscriptEntry::Error { text } => text.clone(),
        TranscriptEntry::Message { text, .. } => format!("Message: {text}"),
        TranscriptEntry::Reasoning { text, .. } => text.clone(),
        TranscriptEntry::Compaction {
            reason,
            threshold,
            context_window_tokens,
            estimated_tokens_before,
            estimated_tokens_after,
            compacted_range_start,
            compacted_range_end,
            checkpoint_id,
            summary,
        } => {
            let reason = match reason {
                event_bus::CompactionReason::Automatic => "automatic",
                event_bus::CompactionReason::Manual => "manual",
                event_bus::CompactionReason::Agent => "agent",
            };
            format!(
                "Compaction ({reason}): {estimated_tokens_before} → {estimated_tokens_after} tokens\nrange {compacted_range_start}..{compacted_range_end}; threshold {threshold}; context window {context_window_tokens}\ncheckpoint {checkpoint_id}\n{summary}"
            )
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
        } => {
            let prefix = match direction {
                MessageDirection::Incoming => "<-",
                MessageDirection::Outgoing => "->",
            };
            format!("{prefix} {peer_run_id}: {content}")
        }
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
        assert!(harness.query_by_label("- seq 1: First entry").is_none());
        // When: the ledger header is expanded.
        harness.get_by_label("> Ledger").click();
        harness.run_steps(2);
        // Then: rows follow the transcript in oldest-first order.
        let first = harness.get_by_label("- seq 1: First entry").rect();
        let last = harness.get_by_label("- seq 3: Last entry").rect();
        assert!(first.top() > harness.get_by_label("Transcript content").rect().bottom());
        assert!(last.top() > first.bottom());
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
        assert!(harness.query_by_label("> Ledger").is_none());
    }

    #[test]
    fn thread_header_omits_phase_pill() {
        for (phase, label) in [
            (ThreadRunPhase::Running, "running"),
            (ThreadRunPhase::Waiting, "waiting (input)"),
            (ThreadRunPhase::Done, "done"),
            (ThreadRunPhase::Stopped, "stopped (resumable)"),
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
