use std::collections::BTreeMap;
use std::time::UNIX_EPOCH;

use event_bus::{Event, EventKind, LedgerEvent};
use storage::RunLedgerEntry;

#[derive(Debug, Default)]
pub struct LedgerRegistry {
    entries: BTreeMap<String, Vec<RunLedgerEntry>>,
}

impl LedgerRegistry {
    pub fn apply(&mut self, event: &Event) {
        let EventKind::Ledger(LedgerEvent::RunLedgerAppended { run_id, seq, body }) = &event.kind
        else {
            return;
        };
        let created_at_ns = event
            .meta
            .wall_clock
            .duration_since(UNIX_EPOCH)
            .map(|duration| i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX))
            .unwrap_or_default();
        self.insert(RunLedgerEntry {
            seq: *seq,
            run_id: run_id.clone(),
            body: body.clone(),
            created_at_ns,
        });
    }

    pub fn load_all(&mut self, entries: Vec<RunLedgerEntry>) {
        self.entries.clear();
        for entry in entries {
            self.insert(entry);
        }
    }

    pub fn entries(&self, run_id: &str) -> &[RunLedgerEntry] {
        self.entries
            .get(run_id)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    fn insert(&mut self, entry: RunLedgerEntry) {
        let entries = self.entries.entry(entry.run_id.clone()).or_default();
        if let Err(index) = entries.binary_search_by_key(&entry.seq, |entry| entry.seq) {
            entries.insert(index, entry);
        }
    }
}
