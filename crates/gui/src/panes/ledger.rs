use crate::theme::tokens::{SURFACE, TEXT};
use crate::theme::widgets::surface_frame;

pub(super) fn ledger_section(ui: &mut egui::Ui, run_id: &str, entries: &[storage::RunLedgerEntry]) {
    if entries.is_empty() {
        return;
    }
    let id = ui.id().with(("ledger-expanded", run_id));
    let mut expanded = ui.data(|data| data.get_temp::<bool>(id).unwrap_or(false));
    surface_frame(SURFACE).show(ui, |ui| {
        let arrow = if expanded { "v" } else { ">" };
        if ui
            .add(
                egui::Button::new(egui::RichText::new(format!("{arrow} Ledger")).color(TEXT))
                    .frame(false)
                    .wrap(),
            )
            .clicked()
        {
            expanded = !expanded;
            ui.data_mut(|data| data.insert_temp(id, expanded));
        }
        if expanded {
            for entry in entries {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(format!("- seq {}: {}", entry.seq, entry.body))
                            .color(TEXT),
                    )
                    .wrap(),
                );
            }
        }
    });
}
