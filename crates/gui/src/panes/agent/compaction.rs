use egui::{Id, Label, RichText, Sense, Ui, collapsing_header::CollapsingState};

use crate::format::compact_count;
use crate::model::transcript::TranscriptEntry;
use crate::theme::tokens::palette;
use crate::theme::widgets::copy_id_button;

pub(super) fn summary(entry: &TranscriptEntry) -> Option<String> {
    let TranscriptEntry::Compaction {
        reason,
        estimated_tokens_before,
        estimated_tokens_after,
        ..
    } = entry
    else {
        return None;
    };
    let reason = match reason {
        event_bus::CompactionReason::Automatic => "automatic",
        event_bus::CompactionReason::Manual => "manual",
        event_bus::CompactionReason::Agent => "agent",
        event_bus::CompactionReason::ModelSwitch => "model switch",
    };
    Some(format!(
        "Context compacted ({reason}): {} → {} tokens",
        compact_count(*estimated_tokens_before),
        compact_count(*estimated_tokens_after)
    ))
}

pub(super) fn show(ui: &mut Ui, id: Id, entry: &TranscriptEntry) {
    let (
        Some(summary),
        TranscriptEntry::Compaction {
            threshold,
            context_window_tokens,
            compacted_range_start,
            compacted_range_end,
            checkpoint_id,
            summary: text,
            ..
        },
    ) = (summary(entry), entry)
    else {
        return;
    };
    ui.add(Label::new(RichText::new(summary).color(palette().TEXT)).wrap());
    // CollapsingHeader forces Extend, widening the shared transcript and all
    // later cards. A custom header keeps long summaries wrapped.
    let mut state = CollapsingState::load_with_default_open(ui.ctx(), id, false);
    let header = ui.horizontal(|ui| {
        state.show_toggle_button(ui, egui::collapsing_header::paint_default_icon);
        ui.add(
            Label::new(RichText::new("details").color(palette().TEXT_MUTED)).sense(Sense::click()),
        )
    });
    if header.inner.clicked() {
        state.toggle(ui);
    }
    state.show_body_indented(&header.response, ui, |ui| {
        ui.visuals_mut().override_text_color = Some(palette().TEXT_MUTED);
        ui.add(
            Label::new(format!(
                "Messages {compacted_range_start}–{compacted_range_end} summarized at {:.0}% of a {}-token window",
                threshold * 100.0,
                compact_count(*context_window_tokens)
            ))
            .wrap(),
        );
        ui.horizontal(|ui| {
            ui.label("Checkpoint saved");
            copy_id_button(ui, "Copy checkpoint ID", checkpoint_id);
        });
        ui.add(Label::new(text.as_str()).wrap());
    });
}
