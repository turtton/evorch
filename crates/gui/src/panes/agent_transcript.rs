use crate::model::transcript::TranscriptModel;
use std::path::Path;

use crate::panes::agent::transcript_body_with_agents;
use crate::panes::transcript_widgets::{run_transcript_empty, run_transcript_title};

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
    run_transcript_title(ui, run_id, agent_name);
    match transcript.filter(|model| !model.entries().is_empty()) {
        Some(model) => transcript_body_with_agents(ui, model, repo_root, agents),
        None => run_transcript_empty(ui, agent_name),
    }
}
