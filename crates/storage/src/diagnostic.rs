//! Explicit and automatic retention for disposable diagnostic events.

/// Which ordinary diagnostic events to remove. Sandbox escalation audit records
/// and unrecognized payloads are always retained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticCleanupScope {
    All,
    /// Exclusive cutoff, in Unix wall-clock nanoseconds.
    Before(i64),
}

/// A preview or completed deletion, with physical storage recovery reported
/// separately from the serialized payload bytes selected/removed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiagnosticCleanupSummary {
    pub event_count: u64,
    pub payload_bytes: u64,
    /// Observed DB + WAL + SHM reduction; always zero in a preview.
    pub reclaimed_bytes: u64,
    /// This legacy database cannot shrink through incremental vacuum. Deleted
    /// pages remain reusable internally; a full offline VACUUM is needed to
    /// return them to the filesystem.
    pub requires_full_vacuum: bool,
    /// Current storage-limit state, reevaluated after an applied cleanup.
    pub writes_suspended: bool,
    /// Some deletion batches committed, but later deletion, reclamation or size
    /// checking could not finish. A caller must report event_count as the exact
    /// successfully removed count; remaining diagnostics may need another cleanup.
    pub maintenance_error: Option<String>,
}

/// Ordinary diagnostic retention; audit records are exempt.
pub const DIAGNOSTIC_RETENTION_DAYS: u64 = 30;
