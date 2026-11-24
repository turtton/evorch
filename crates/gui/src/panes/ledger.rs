use crate::panes::run_card_widgets::{NoteRevision, working_note};

pub(super) fn ledger_section(ui: &mut egui::Ui, run_id: &str, entries: &[storage::RunLedgerEntry]) {
    let revisions: Vec<_> = entries
        .iter()
        .map(|entry| NoteRevision {
            seq: entry.seq,
            body: &entry.body,
        })
        .collect();
    working_note(ui, run_id, &revisions, false);
}
