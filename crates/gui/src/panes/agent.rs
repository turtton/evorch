//! Agent トランスクリプトペインの描画。

use crate::model::transcript::{TranscriptEntry, TranscriptModel};

/// トランスクリプトモデルを egui 上に描画します。
pub fn agent_pane(ui: &mut egui::Ui, model: &TranscriptModel) {
    ui.heading("Agent");
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
            }
        }
    });
}
