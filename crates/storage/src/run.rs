//! Persisted run ledger entries and terminal context snapshots.

/// An immutable entry in the globally sequenced run ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunLedgerEntry {
    pub seq: u64,
    pub run_id: String,
    pub body: String,
    pub created_at_ns: i64,
}

/// The latest run snapshot. JSON payloads are opaque to storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunContextRecord {
    pub run_id: String,
    pub role: String,
    pub name: String,
    pub parent_run_id: Option<String>,
    pub config_json: String,
    pub messages_json: String,
    pub checkpoints_json: String,
    pub terminal_phase: String,
    pub restorable: bool,
    pub updated_at_ns: i64,
}
