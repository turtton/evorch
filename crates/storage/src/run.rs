//! Persisted working note revisions and terminal context snapshots.

/// An immutable complete working note revision; an empty body explicitly clears it.
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
    /// JSON array of the prompt IDs of host inputs this snapshot's messages
    /// contain, kept after a compaction summarizes them. `[]` when none.
    pub host_inputs_json: String,
}

/// Identity of the redacted, committed snapshot (not an authority token).
/// Host input IDs are left out: they never change the restored messages.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RunContextReceipt {
    pub updated_at_ns: i64,
    pub messages_end: usize,
    pub sha256: String,
}

impl RunContextReceipt {
    /// Missing, replaced or power-loss-truncated snapshots are unavailable.
    pub fn matches(&self, record: &RunContextRecord) -> bool {
        use sha2::{Digest, Sha256};
        self.updated_at_ns == record.updated_at_ns
            && self.sha256 == format!("{:x}", Sha256::digest(record.messages_json.as_bytes()))
            && serde_json::from_str::<Vec<serde_json::Value>>(&record.messages_json)
                .is_ok_and(|messages| messages.len() == self.messages_end)
    }
}
