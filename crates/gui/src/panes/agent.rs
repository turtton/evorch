//! Agent トランスクリプトペインの描画。

use egui::Color32;
use workspace_ui::ThreadRunPhase;

use crate::model::composer::ComposerModel;
use crate::model::transcript::{MessageDirection, TranscriptEntry, TranscriptModel};
use crate::panes::agents::AgentsAction;
use crate::panes::composer::{ComposerAction, composer_strip};
use crate::panes::sidebar::SidebarAction;
use crate::theme::text::h3;
use crate::theme::tokens::*;
use crate::theme::widgets::{card, empty_state, pane_root, surface_frame};

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
    pub task_rows: &'a [crate::model::tasks::TaskRow],
    pub phase_unread: bool,
    pub has_project: bool,
    pub active_thread_title: Option<&'a str>,
    pub phase: Option<ThreadRunPhase>,
    pub next_thread_title: String,
    pub model_picker: crate::panes::model_picker::ModelPickerContext<'a>,
}

/// Agent 会話ペインから発生するアクションです。
pub enum AgentPaneAction {
    Agents(AgentsAction),
    Sidebar(SidebarAction),
    FocusPanel(&'static str),
    Composer(ComposerAction),
    ModelPreference(Option<workspace_ui::ModelPreference>),
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
    pane_root(ui, "Conversation", |ui| {
        let mut action = None;
        header_strip(ui, &identity, &ctx, &mut action);
        let composer_id = ui.id().with("composer-height");
        let composer_height = ui
            .data(|data| data.get_temp::<f32>(composer_id))
            .unwrap_or(COMPOSER_MIN_HEIGHT);
        egui::Panel::bottom(ui.id().with("composer"))
            .exact_size(composer_height)
            .resizable(false)
            .show_separator_line(false)
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
                let strip = ui.scope(|ui| {
                    ui.push_id("composer-strip", |ui| {
                        composer_strip(ui, composer, ctx.model_picker, picker_state, ctx.phase)
                    })
                    .inner
                });
                let height = strip.response.rect.height();
                if height != composer_height {
                    ui.data_mut(|data| data.insert_temp(composer_id, height));
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
                    && identity.is_none_or(|identity| identity.ledger.is_empty())
                {
                    empty_state_body(ui, &ctx, &mut action);
                } else {
                    run_detail_body(ui, model, (identity, ctx.task_rows));
                }
            });
        action
    })
}

fn header_strip(
    ui: &mut egui::Ui,
    identity: &Option<AgentIdentity<'_>>,
    ctx: &ConversationContext<'_>,
    action: &mut Option<AgentPaneAction>,
) {
    if identity.is_none() && ctx.active_thread_title.is_none() {
        return;
    }
    surface_frame(palette().SURFACE).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.set_min_height(ROW_COMPACT - 2.0 * SP_2);
            if let Some(identity) = identity {
                let label = match (identity.name, identity.role) {
                    (Some(name), Some(role)) => {
                        format!("{} / {name} / {role}", identity.run_id)
                    }
                    (Some(name), None) => format!("{} / {name}", identity.run_id),
                    (None, Some(role)) => format!("{} / {role}", identity.run_id),
                    (None, None) => identity.run_id.to_owned(),
                };
                ui.label(h3(label));
                if ui.button("← Thread").clicked() {
                    *action = Some(AgentPaneAction::Agents(AgentsAction::ReturnToThread));
                }
            } else if let Some(title) = ctx.active_thread_title {
                ui.label(h3(format!("Thread: {title}")));
            }
            if let Some(phase) = ctx.phase {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    crate::panes::phase_indicator::phase_indicator_with_ack(
                        ui,
                        phase,
                        ctx.phase_unread,
                    );
                });
            }
        });
    });
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
            "Type a message below, or /goal <text> to start the loop.",
            None,
        );
    }
}

pub fn transcript_body(ui: &mut egui::Ui, model: &TranscriptModel) {
    run_detail_body(ui, model, (None, &[]));
}

fn run_detail_body(
    ui: &mut egui::Ui,
    model: &TranscriptModel,
    context: (Option<AgentIdentity<'_>>, &[crate::model::tasks::TaskRow]),
) {
    let (identity, task_rows) = context;
    let pane_id = ui.id();
    egui::ScrollArea::vertical()
        .stick_to_bottom(true)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for (entry_idx, entry) in model.visible_entries().iter().enumerate() {
                if matches!(entry, TranscriptEntry::Tool { .. }) {
                    crate::panes::transcript_tool::tool_card(ui, entry, pane_id);
                    continue;
                }
                let accent = entry_accent(entry);
                card(ui, accent, |ui| {
                    match entry {
                        TranscriptEntry::Message { run_id, .. }
                        | TranscriptEntry::Reasoning { run_id, .. } => {
                            if let Some(role) = run_id.as_deref().and_then(|run_id| {
                                crate::model::tasks::role_for_run(task_rows, run_id)
                            }) {
                                ui.label(
                                    egui::RichText::new(format!("[{role}]"))
                                        .small()
                                        .color(palette().TEXT_MUTED),
                                );
                            }
                        }
                        TranscriptEntry::Error { .. }
                        | TranscriptEntry::UserMessage { .. }
                        | TranscriptEntry::Notice { .. }
                        | TranscriptEntry::Compaction { .. }
                        | TranscriptEntry::Tool { .. }
                        | TranscriptEntry::AgentMessage { .. } => {}
                    }
                    if let TranscriptEntry::Message { text, .. } = entry {
                        crate::panes::markdown_render::render_markdown(
                            ui,
                            text,
                            &format!("msg-{entry_idx}"),
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
        });
}

fn entry_accent(entry: &TranscriptEntry) -> Color32 {
    match entry {
        TranscriptEntry::Error { .. } => palette().ERROR_FG,
        TranscriptEntry::UserMessage { .. } => palette().TEXT,
        TranscriptEntry::Notice { .. } | TranscriptEntry::Compaction { .. } => palette().TEXT_MUTED,
        TranscriptEntry::Message { .. } => palette().ACCENT,
        TranscriptEntry::Reasoning { .. } => palette().TEXT_MUTED,
        TranscriptEntry::Tool { .. } => palette().INFO,
        TranscriptEntry::AgentMessage { direction, .. } => match direction {
            MessageDirection::Incoming => palette().SUCCESS,
            MessageDirection::Outgoing => palette().WARNING_FG,
        },
    }
}

fn entry_label(entry: &TranscriptEntry) -> String {
    match entry {
        TranscriptEntry::UserMessage { text } => format!("You: {text}"),
        TranscriptEntry::Notice { text } | TranscriptEntry::Error { text } => text.clone(),
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
            );
        });
        // Then: no ledger header is exposed.
        assert!(harness.query_by_label("> Ledger").is_none());
    }

    #[test]
    fn thread_header_shows_phase_pill_without_clipping() {
        for (phase, label) in [
            (ThreadRunPhase::Running, "running"),
            (ThreadRunPhase::Waiting, "waiting (input)"),
            (ThreadRunPhase::Done, "done"),
        ] {
            let mut harness = Harness::builder()
                .with_size(egui::vec2(400.0, 80.0))
                .build_ui(move |ui| {
                    crate::theme::install(ui.ctx());
                    let ctx = ConversationContext {
                        task_rows: &[],
                        phase_unread: true,
                        has_project: true,
                        active_thread_title: Some("Chat"),
                        phase: Some(phase),
                        next_thread_title: String::new(),
                        model_picker: crate::panes::model_picker::ModelPickerContext {
                            profiles: &[],
                            preference: None,
                            enabled: false,
                        },
                    };
                    header_strip(ui, &None, &ctx, &mut None);
                });
            harness.run_steps(2);
            let pill = harness.get_by_label(label).rect();
            assert!(pill.left() > harness.get_by_label("Thread: Chat").rect().right());
            assert!(pill.right() < 400.0 && pill.bottom() < 80.0);
        }
    }
}
