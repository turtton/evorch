//! Agent トランスクリプトペインの描画。

use egui::Color32;
use workspace_ui::ThreadRunPhase;

use crate::model::composer::{ComposerModel, ProviderStatus};
use crate::model::transcript::{MessageDirection, TranscriptEntry, TranscriptModel};
use crate::panes::agents::AgentsAction;
use crate::panes::composer::{ComposerAction, composer_strip};
use crate::panes::phase_indicator::phase_indicator;
use crate::panes::sidebar::SidebarAction;
use crate::theme::text::h3;
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
    provider: &ProviderStatus,
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
                    if let Some(preference) = crate::panes::model_picker::model_picker(
                        ui,
                        crate::panes::model_picker::ModelPickerContext {
                            profiles: ctx.model_picker.profiles,
                            preference: ctx.model_picker.preference,
                            enabled: ctx.model_picker.enabled,
                        },
                        picker_state,
                    ) {
                        action = Some(AgentPaneAction::ModelPreference(preference));
                    }
                    ui.push_id("composer-strip", |ui| {
                        composer_strip(ui, composer, provider)
                    })
                    .inner
                });
                let height = strip.response.rect.height();
                if height != composer_height {
                    ui.data_mut(|data| data.insert_temp(composer_id, height));
                    ui.ctx().request_repaint();
                }
                if let Some(composer_action) = strip.inner {
                    action = Some(AgentPaneAction::Composer(composer_action));
                }
            });
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
                if model.visible_entries().is_empty() {
                    empty_state_body(ui, &ctx, &mut action);
                } else {
                    transcript_body(ui, model);
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
                        phase_indicator(ui, phase);
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
    egui::ScrollArea::vertical()
        .stick_to_bottom(true)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for entry in model.visible_entries() {
                let accent = entry_accent(entry);
                let text = entry_label(entry);
                card(ui, accent, |ui| {
                    let foreground = match entry {
                        TranscriptEntry::Error { .. } => ERROR_FG,
                        _ => TEXT,
                    };
                    ui.label(egui::RichText::new(text).color(foreground));
                });
            }
        });
}

fn entry_accent(entry: &TranscriptEntry) -> Color32 {
    match entry {
        TranscriptEntry::Error { .. } => ERROR_FG,
        TranscriptEntry::UserMessage { .. } => TEXT,
        TranscriptEntry::Notice { .. } => TEXT_MUTED,
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
        TranscriptEntry::UserMessage { text } => format!("You: {text}"),
        TranscriptEntry::Notice { text } | TranscriptEntry::Error { text } => text.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::{Harness, kittest::Queryable};

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
