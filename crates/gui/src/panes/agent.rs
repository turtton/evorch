//! Agent トランスクリプトペインの描画。

use crate::model::transcript::{MessageDirection, TranscriptEntry, TranscriptModel};

/// トランスクリプトモデルを egui 上に描画します。
pub fn agent_pane(ui: &mut egui::Ui, model: &TranscriptModel) {
    ui.heading("Conversation");
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
