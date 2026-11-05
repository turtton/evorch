//! The candidate chip under a tool output (ADR 0043): "Link PR / Ignore" for a pull request the
//! output named, and a one-line confirmation once the thread is linked to it.
//!
//! The label is built from the parsed `owner/repo#number`; the tool output's own text is never
//! shown here.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use egui::RichText;

use crate::model::pull_requests::{CandidateChip, ChipState, PullRequestAction};
use crate::model::transcript::TranscriptEntry;
use crate::theme::icons;
use crate::theme::text::muted;
use crate::theme::tokens::*;
use crate::theme::widgets::ghost;

pub const LINK_LABEL: &str = "Link PR";
pub const IGNORE_LABEL: &str = "Ignore";

/// Chips by `(run, call)` of the tool call that produced them.
pub type ChipsByCall = HashMap<(String, String), Vec<CandidateChip>>;

/// What the transcript needs to draw the chips and report a click.
pub struct PullRequestChips<'a> {
    pub thread_id: &'a str,
    pub by_call: &'a ChipsByCall,
    pub action: &'a mut Option<PullRequestAction>,
}

/// Changes whenever a chip appears, changes or goes, so cached row heights are not reused.
#[must_use]
pub fn revision(by_call: &ChipsByCall) -> u64 {
    let mut entries: Vec<_> = by_call.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    let mut hasher = std::hash::DefaultHasher::new();
    for (key, chips) in entries {
        key.hash(&mut hasher);
        for chip in chips {
            (chip.candidate_id, chip.state as u8).hash(&mut hasher);
        }
    }
    hasher.finish()
}

fn chips_of<'a>(by_call: &'a ChipsByCall, run: Option<&str>, call: &str) -> Vec<&'a CandidateChip> {
    match run {
        Some(run) => by_call
            .get(&(run.to_owned(), call.to_owned()))
            .into_iter()
            .flatten()
            .collect(),
        None => by_call
            .iter()
            .filter(|((_, candidate_call), _)| candidate_call == call)
            .flat_map(|(_, chips)| chips)
            .collect(),
    }
}

/// Draws the chips of every tool call in `group`, directly under its card.
pub fn show(ui: &mut egui::Ui, chips: &mut PullRequestChips<'_>, group: &[TranscriptEntry]) {
    for entry in group {
        let TranscriptEntry::Tool {
            run_id, call_id, ..
        } = entry
        else {
            continue;
        };
        for chip in chips_of(chips.by_call, run_id.as_deref(), call_id) {
            ui.horizontal(|ui| {
                ui.add_space(SP_2);
                ui.label(RichText::new(icons::GIT_PULL_REQUEST).color(palette().TEXT_MUTED));
                match chip.state {
                    ChipState::Linked => {
                        ui.label(muted(format!("Linked pull request {}", chip.label)));
                    }
                    ChipState::Offered => {
                        ui.label(&chip.label);
                        let link = ui.add(ghost(RichText::new(LINK_LABEL).color(palette().ACCENT)));
                        link.widget_info(|| {
                            egui::WidgetInfo::labeled(
                                egui::WidgetType::Button,
                                true,
                                format!("{LINK_LABEL} {}", chip.label),
                            )
                        });
                        if link.clicked() {
                            *chips.action = Some(PullRequestAction::Confirm {
                                thread_id: chips.thread_id.to_owned(),
                                candidate_id: chip.candidate_id,
                            });
                        }
                        let ignore = ui.add(ghost(muted(IGNORE_LABEL)));
                        if ignore.clicked() {
                            *chips.action = Some(PullRequestAction::Dismiss {
                                thread_id: chips.thread_id.to_owned(),
                                candidate_id: chip.candidate_id,
                            });
                        }
                    }
                }
            });
        }
    }
}
