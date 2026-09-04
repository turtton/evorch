use crate::model::transcript::TranscriptModel;
use crate::panes::agent::transcript_body;

pub fn agent_transcript_pane(
    ui: &mut egui::Ui,
    run_id: &str,
    transcript: Option<&TranscriptModel>,
) {
    ui.heading(format!("Transcript: {run_id}"));
    match transcript {
        Some(model) if model.entries().is_empty() => {
            ui.label(format!("no events for {run_id}"));
        }
        Some(model) => transcript_body(ui, model),
        None => {
            ui.label(format!("no events for {run_id}"));
        }
    }
}
