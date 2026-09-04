//! Run-scoped transcript pane stub.

use crate::model::transcript::TranscriptModel;

pub fn agent_transcript_pane(
    ui: &mut egui::Ui,
    run_id: &str,
    transcript: Option<&TranscriptModel>,
) {
    ui.heading(format!("Transcript: {run_id}"));
    if let Some(model) = transcript {
        ui.label(format!("{} transcript entries", model.entries().len()));
    } else {
        ui.label(format!("no transcript for {run_id}"));
    }
}
