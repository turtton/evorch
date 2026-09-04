//! Agent トランスクリプトペインの描画。

use crate::model::transcript::{MessageDirection, TranscriptEntry, TranscriptModel};
use crate::panes::agents::AgentsAction;

#[derive(Debug, Clone, Copy)]
pub struct AgentIdentity<'a> {
    pub run_id: &'a str,
    pub name: Option<&'a str>,
    pub role: Option<&'a str>,
}

/// トランスクリプトモデルを egui 上に描画します。
pub fn agent_pane(
    ui: &mut egui::Ui,
    model: &TranscriptModel,
    identity: Option<AgentIdentity<'_>>,
) -> Option<AgentsAction> {
    ui.heading("Conversation");
    let action = identity.and_then(|identity| {
        ui.horizontal(|ui| {
            let label = match (identity.name, identity.role) {
                (Some(name), Some(role)) => format!("{} / {name} / {role}", identity.run_id),
                (Some(name), None) => format!("{} / {name}", identity.run_id),
                (None, Some(role)) => format!("{} / {role}", identity.run_id),
                (None, None) => identity.run_id.to_owned(),
            };
            ui.label(label);
            ui.button("← Thread")
                .clicked()
                .then_some(AgentsAction::ReturnToThread)
        })
        .inner
    });
    transcript_body(ui, model);
    action
}

pub fn transcript_body(ui: &mut egui::Ui, model: &TranscriptModel) {
    egui::ScrollArea::vertical().show(ui, |ui| {
        for entry in model.visible_entries() {
            match entry {
                TranscriptEntry::Message { text } => {
                    ui.label(format!("Message: {text}"));
                }
                TranscriptEntry::Reasoning { text } => {
                    ui.label(format!("Reasoning: {text}"));
                }
                TranscriptEntry::Tool {
                    tool_name,
                    call_id,
                    status,
                } => {
                    ui.label(format!("Tool {tool_name} ({call_id}): {status:?}"));
                }
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
                    ui.label(format!("{prefix} {peer_run_id}: {content}"));
                }
            }
        }
    });
}
