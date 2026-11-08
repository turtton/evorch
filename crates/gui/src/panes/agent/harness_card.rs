//! The card of a harness notice in the transcript (ADR 0043 Phase 2): the `harness` tag, the
//! pull request and commit, the facts and the log excerpts. Nothing else: the wrapper the agent
//! reads is never drawn, and the card carries no caution about its data.

use egui::{RichText, Ui};

use crate::model::harness_card::HarnessCard;
use crate::model::pull_requests::short_sha;
use crate::theme::text::{medium, muted};
use crate::theme::tokens::*;
use crate::theme::widgets::soft_frame;

/// The tag every card starts with.
pub const TAG: &str = "harness";

/// The line that names the pull request and the commit.
#[must_use]
pub fn headline(card: &HarnessCard) -> String {
    format!(
        "{}#{} · {}",
        card.repository,
        card.number,
        short_sha(&card.head_sha)
    )
}

/// Draws one card; a notice whose card is not read yet shows the tag alone.
pub(super) fn show(ui: &mut Ui, id: egui::Id, card: Option<&HarnessCard>) {
    soft_frame(palette().SURFACE).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = SP_1;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = SP_2;
            egui::Frame::new()
                .stroke(egui::Stroke::new(STATUS_STROKE, palette().TEXT_MUTED))
                .corner_radius(egui::CornerRadius::same(R_SM))
                .inner_margin(egui::Margin::symmetric(4, 0))
                .show(ui, |ui| {
                    ui.label(
                        RichText::new(TAG)
                            .size(FONT_BADGE)
                            .color(palette().TEXT_MUTED),
                    );
                });
            let Some(card) = card else { return };
            ui.label(medium(headline(card))).on_hover_text(format!(
                "{}#{} {}",
                card.repository, card.number, card.head_sha
            ));
        });
        let Some(card) = card else { return };
        if !card.checks.is_empty() {
            ui.label(muted(format!("Failed: {}", card.checks.join(", "))));
        }
        if let Some(at) = &card.observed_at {
            ui.label(muted(format!("Observed {at}")));
        }
        for (index, log) in card.logs.iter().enumerate() {
            if !log.checks.is_empty() {
                ui.label(muted(log.checks.join(", ")));
            }
            egui::ScrollArea::vertical()
                .id_salt(id.with(("log", index)))
                .max_height(LOG_MAX_HEIGHT)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.add(
                        egui::Label::new(RichText::new(&log.excerpt).monospace().size(FONT_SMALL))
                            .wrap(),
                    );
                });
        }
        if card.logs_truncated {
            ui.label(muted("Log shortened"));
        }
    });
}

/// The tallest one log excerpt grows before it scrolls.
const LOG_MAX_HEIGHT: f32 = 160.0;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::harness_card::HarnessLog;
    use egui_kittest::{Harness, kittest::NodeT};

    fn card() -> HarnessCard {
        HarnessCard {
            notice_id: "prn-1".into(),
            repository: "turtton/evorch".into(),
            number: 12,
            head_sha: "a".repeat(40),
            manual: false,
            checks: vec!["unit: failure".into(), "lint: timed out".into()],
            observed_at: Some("2026-01-01T00:00:00Z".into()),
            logs: vec![HarnessLog {
                checks: vec!["unit".into()],
                excerpt: "error[E0308]: mismatched types\n --> src/lib.rs:3".into(),
            }],
            logs_truncated: true,
        }
    }

    fn labels(card: Option<HarnessCard>) -> Vec<String> {
        let mut harness = Harness::builder()
            .with_size(egui::vec2(700.0, 400.0))
            .build_ui(move |ui| {
                crate::theme::install(ui.ctx());
                show(ui, egui::Id::new("card"), card.as_ref());
            });
        harness.run_steps(3);
        harness
            .root()
            .children_recursive()
            .flat_map(|node| {
                let node = node.accesskit_node();
                [node.label(), node.value()]
            })
            .flatten()
            .map(|text| text.to_owned())
            .collect()
    }

    // Given: a card of a notice / When: it is drawn / Then: the tag, the pull request and short
    // commit, the facts and the log excerpt are there, and nothing the agent reads is
    #[test]
    fn a_card_shows_the_tag_the_pull_request_the_facts_and_the_excerpt_only() {
        let all = labels(Some(card()));
        let has = |text: &str| all.iter().any(|label| label.contains(text));
        assert!(has(TAG) && has("turtton/evorch#12 · aaaaaaa"), "{all:?}");
        assert!(has("Failed: unit: failure, lint: timed out"), "{all:?}");
        assert!(has("Observed 2026-01-01T00:00:00Z"), "{all:?}");
        assert!(has("error[E0308]: mismatched types"), "{all:?}");
        for label in &all {
            let lower = label.to_lowercase();
            for forbidden in [
                "[harness-notice",
                "<ci-log-data>",
                "data_not_instructions",
                "untrusted",
                "external data",
                "not from the user",
                "信頼",
                "ユーザー発言",
            ] {
                assert!(!lower.contains(forbidden), "{forbidden:?} in {label:?}");
            }
        }
    }

    // Writes the card as a PNG when `EVORCH_PR_BAR_CAPTURE_DIR` names a directory.
    #[test]
    fn the_card_renders_for_evidence() {
        let Some(directory) = std::env::var_os("EVORCH_PR_BAR_CAPTURE_DIR") else {
            return;
        };
        let directory = std::path::PathBuf::from(directory);
        std::fs::create_dir_all(&directory).unwrap();
        let shown = card();
        let mut harness = Harness::builder()
            .with_size(egui::vec2(700.0, 260.0))
            .build_ui(move |ui| {
                crate::theme::install(ui.ctx());
                show(ui, egui::Id::new("card"), Some(&shown));
            });
        harness.run_steps(4);
        harness
            .render()
            .unwrap()
            .save(directory.join("harness-card.png"))
            .unwrap();
    }

    // Given: a notice whose card is not read yet / Then: the tag stands alone
    #[test]
    fn a_card_that_is_not_read_yet_is_the_tag_alone() {
        let all = labels(None);
        assert!(all.iter().any(|label| label == TAG), "{all:?}");
        assert!(
            !all.iter().any(|label| label.contains("turtton")),
            "{all:?}"
        );
    }
}
