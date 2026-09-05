//! Agent トランスクリプトペインの描画。

use egui::Color32;
use workspace_ui::ThreadRunPhase;

use crate::model::transcript::{MessageDirection, TranscriptEntry, TranscriptModel};
use crate::panes::agents::AgentsAction;
use crate::panes::sidebar::SidebarAction;
use crate::theme::text::{h3, muted};
use crate::theme::tokens::*;
use crate::theme::widgets::{card, empty_state, pane_root, surface_frame};

#[derive(Debug, Clone, Copy)]
pub struct AgentIdentity<'a> {
    pub run_id: &'a str,
    pub name: Option<&'a str>,
    pub role: Option<&'a str>,
}

/// 会話ペインが描画される文脈です。
pub struct ConversationContext<'a> {
    pub has_project: bool,
    pub active_thread_title: Option<&'a str>,
    pub phase: Option<ThreadRunPhase>,
    pub next_thread_title: String,
}

/// Agent 会話ペインから発生するアクションです。
pub enum AgentPaneAction {
    Agents(AgentsAction),
    Sidebar(SidebarAction),
    FocusPanel(&'static str),
}

/// トランスクリプトモデルを egui 上に描画します。
pub fn agent_pane(
    ui: &mut egui::Ui,
    model: &TranscriptModel,
    identity: Option<AgentIdentity<'_>>,
    ctx: ConversationContext<'_>,
) -> Option<AgentPaneAction> {
    pane_root(ui, "Conversation", |ui| {
        let mut action = None;
        header_strip(ui, &identity, &ctx, &mut action);
        if model.visible_entries().is_empty() {
            empty_state_body(ui, &ctx, &mut action);
        } else {
            transcript_body(ui, model);
        }
        footer_strip(ui);
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
    surface_frame(SURFACE).show(ui, |ui| {
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
                ui.label(egui::RichText::new(label).color(TEXT));
                if ui.button("← Thread").clicked() {
                    *action = Some(AgentPaneAction::Agents(AgentsAction::ReturnToThread));
                }
            } else if let Some(title) = ctx.active_thread_title {
                ui.label(h3(format!("Thread: {title}")));
                if let Some(phase) = ctx.phase {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(muted(format!("{phase:?}").to_lowercase()));
                    });
                }
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
    } else if empty_state(
        ui,
        "No messages yet",
        "Submit a goal to start the run.",
        Some("Go to Goal"),
    ) {
        *action = Some(AgentPaneAction::FocusPanel("goal-main"));
    }
}

pub fn transcript_body(ui: &mut egui::Ui, model: &TranscriptModel) {
    egui::ScrollArea::vertical()
        .stick_to_bottom(true)
        .show(ui, |ui| {
            for entry in model.visible_entries() {
                let accent = entry_accent(entry);
                let text = entry_label(entry);
                card(ui, accent, |ui| {
                    ui.label(egui::RichText::new(text).color(TEXT));
                });
            }
        });
}

fn entry_accent(entry: &TranscriptEntry) -> Color32 {
    match entry {
        TranscriptEntry::Message { .. } => ACCENT,
        TranscriptEntry::Reasoning { .. } => TEXT_MUTED,
        TranscriptEntry::Tool { .. } => INFO,
        TranscriptEntry::AgentMessage { direction, .. } => match direction {
            MessageDirection::Incoming => SUCCESS,
            MessageDirection::Outgoing => WARNING_FG,
        },
    }
}

fn entry_label(entry: &TranscriptEntry) -> String {
    match entry {
        TranscriptEntry::Message { text } => format!("Message: {text}"),
        TranscriptEntry::Reasoning { text } => format!("Reasoning: {text}"),
        TranscriptEntry::Tool {
            tool_name,
            call_id,
            status,
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

fn footer_strip(ui: &mut egui::Ui) {
    surface_frame(SURFACE_RAISED).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.set_min_height(ROW_COMPACT - 2.0 * SP_2);
            ui.label(muted("Goal-driven — compose in the Goal panel"));
        });
    });
}
