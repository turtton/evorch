use crate::model::transcript::TranscriptModel;
use crate::panes::agent::transcript_body;
use crate::theme::text::h3;
use crate::theme::widgets::empty_state;

pub fn agent_transcript_pane(
    ui: &mut egui::Ui,
    run_id: &str,
    transcript: Option<&TranscriptModel>,
) {
    ui.label(h3(format!("Transcript: {run_id}")));
    match transcript.filter(|model| !model.entries().is_empty()) {
        Some(model) => transcript_body(ui, model),
        None => {
            empty_state(
                ui,
                &format!("no events for {run_id}"),
                "Messages and tool activity will appear as this agent runs.",
                None,
            );
        }
    }
}
