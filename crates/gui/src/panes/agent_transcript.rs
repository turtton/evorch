use crate::model::transcript::TranscriptModel;
use std::path::Path;

use crate::panes::agent::transcript_body_with_agents;
use crate::theme::text::h3;
use crate::theme::widgets::{copy_id_button, empty_state};

pub fn agent_transcript_pane(
    ui: &mut egui::Ui,
    run_id: &str,
    transcript: Option<&TranscriptModel>,
) {
    agent_transcript_pane_with_repo_root(ui, run_id, None, transcript, None, &[]);
}

/// `agent_name` titles the pane; the run ID itself stays behind a copy button.
pub fn agent_transcript_pane_with_repo_root(
    ui: &mut egui::Ui,
    run_id: &str,
    agent_name: Option<&str>,
    transcript: Option<&TranscriptModel>,
    repo_root: Option<&Path>,
    agents: &[crate::model::tasks::TaskRow],
) {
    ui.horizontal(|ui| {
        ui.label(h3(match agent_name {
            Some(name) => format!("Transcript: {name}"),
            None => "Transcript".to_owned(),
        }));
        copy_id_button(ui, "Copy run ID", run_id);
    });
    match transcript.filter(|model| !model.entries().is_empty()) {
        Some(model) => transcript_body_with_agents(ui, model, repo_root, agents),
        None => {
            empty_state(
                ui,
                &match agent_name {
                    Some(name) => format!("No events for {name} yet"),
                    None => "No events yet".to_owned(),
                },
                "Messages and tool activity will appear as this agent runs.",
                None,
            );
        }
    }
}
