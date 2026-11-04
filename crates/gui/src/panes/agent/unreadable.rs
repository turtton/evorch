//! A stored event the history could not read, in the place it occupies.

use egui::{RichText, Ui};

use crate::body::resolver::AUTO_RETRIES;
use crate::body::ui::{COPY_DETAILS, RETRY, unreadable_details};
use crate::body::{BodyRef, EventPhase, FailureKind};
use crate::theme::icons;
use crate::theme::tokens::{SP_1, SP_2, palette};

pub(super) struct Gap<'a> {
    pub event_id: i64,
    pub run_id: Option<&'a str>,
    pub kind: &'a FailureKind,
    pub reason: &'a str,
    /// The body whose read failed, when the event itself decodes.
    pub reference: Option<&'a BodyRef>,
}

pub const TITLE_PREFIX: &str = "Couldn't read event";

#[must_use]
pub fn title(event_id: i64, kind: &FailureKind) -> String {
    match kind {
        FailureKind::Unreadable => format!("{TITLE_PREFIX} #{event_id}"),
        FailureKind::Body(_) => format!("{TITLE_PREFIX} #{event_id}: its content is unavailable"),
    }
}

fn kind_name(kind: &FailureKind) -> String {
    match kind {
        FailureKind::Unreadable => "unreadable".to_owned(),
        FailureKind::Body(kind) => format!("body {}", kind.label()),
    }
}

pub(super) fn show(ui: &mut Ui, id: egui::Id, gap: &Gap<'_>) {
    let phase = crate::body::ui::resolver(ui.ctx())
        .map(|resolver| resolver.event_phase(gap.event_id))
        .unwrap_or(EventPhase::Idle);
    let loading = phase == EventPhase::Loading;
    let retrying = match phase {
        EventPhase::Retrying { retries } => Some(retries),
        _ => None,
    };
    let reason = match &phase {
        EventPhase::Failed(latest) => latest.as_str(),
        _ => gap.reason,
    };
    let frame = egui::Frame::new()
        .fill(palette().ERROR_SURFACE)
        .inner_margin(egui::Margin::symmetric(SP_2 as i8, SP_1 as i8))
        .corner_radius(egui::CornerRadius::same(4));
    let mut retry = false;
    let mut copy = false;
    frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.push_id(id, |ui| {
            ui.horizontal_wrapped(|ui| {
                if loading {
                    ui.add(egui::Spinner::new());
                    ui.label(RichText::new("Reading the event again…").color(palette().TEXT_MUTED));
                } else if let Some(retries) = retrying {
                    ui.add(egui::Spinner::new());
                    ui.label(
                        RichText::new(format!(
                            "{} — retrying (retry {retries} of {AUTO_RETRIES})",
                            title(gap.event_id, gap.kind),
                        ))
                        .color(palette().TEXT_MUTED),
                    );
                } else {
                    ui.label(
                        RichText::new(icons::with_icon(
                            icons::WARNING,
                            title(gap.event_id, gap.kind),
                        ))
                        .color(palette().ERROR_FG),
                    );
                    ui.label(RichText::new(reason).color(palette().TEXT_MUTED));
                    retry = ui.button(RETRY).clicked();
                    copy = ui.button(COPY_DETAILS).clicked();
                }
            });
        });
    });
    if copy {
        ui.ctx().copy_text(unreadable_details(
            gap.event_id,
            gap.run_id,
            &kind_name(gap.kind),
            reason,
            gap.reference,
        ));
    }
    if retry && let (Some(resolver), Some(run)) = (crate::body::ui::resolver(ui.ctx()), gap.run_id)
    {
        resolver.request_gap(run, gap.event_id);
    }
}
