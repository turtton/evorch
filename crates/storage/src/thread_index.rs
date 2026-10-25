//! Phase B contracts shared by storage, the writer, runtime and measurement.
//!
//! ADR 0041 is normative. Traits here deliberately have no implementation:
//! migrations, SQL transitions, writer commands, jobs and search are later work.
//! Revisions are per-run monotonic integers, not timestamps; version 0 snapshots
//! are legacy and must not acquire native provenance by inference.

mod contracts;

pub use contracts::*;

use serde::{Deserialize, Serialize};

use crate::{RunContextRecord, RunLedgerEntry, threads::ThreadRunRow};

pub const PROVENANCE_FORMAT_VERSION: u32 = 1;
pub const EXTRACTOR_VERSION: u32 = 1;
pub const TOOL_USE_MAX_BYTES: usize = 4096;
pub const TOOL_RESULT_MAX_BYTES: usize = 2048;

/// Sources are identities only: never duplicate text, secrets or image payloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source_kind", rename_all = "snake_case")]
pub enum CopySource {
    Message {
        run_id: String,
        snapshot_revision: u64,
        ordinal: usize,
        block_index: usize,
    },
    WorkingNote {
        run_id: String,
        note_seq: u64,
    },
    FrozenHistory {
        history_source_id: String,
        record_index: u64,
    },
}

/// The original non-System boundary, never a post-compaction display ordinal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkBoundary {
    pub run_id: String,
    pub snapshot_revision: u64,
    pub context_len: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "origin", rename_all = "snake_case")]
pub enum BlockOrigin {
    Native,
    ContinuityCopy {
        source: CopySource,
    },
    ForkCopy {
        source: CopySource,
        original_origin: Box<BlockOrigin>,
        boundary: ForkBoundary,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExcludedReason {
    GeneratedBoundary,
}

/// Include every block, including hidden/unknown/copy blocks, in original order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockProvenance {
    pub block_index: usize,
    pub origin: BlockOrigin,
    pub excluded_reason: Option<ExcludedReason>,
}

/// Include every non-System message, even one with no indexable content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageProvenance {
    pub raw_index: usize,
    pub ordinal: usize,
    pub blocks: Vec<BlockProvenance>,
}

/// Checkpoint summaries are separate from raw messages and never indexed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvenanceMap {
    pub version: u32,
    pub raw_message_count: usize,
    pub checkpoint_count: usize,
    pub messages: Vec<MessageProvenance>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProvenanceUnknownReason {
    MissingSidecar,
    SnapshotMismatch,
    IncompleteMap,
    LegacyUnproven,
    UnsupportedVersion,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "evidence_kind", rename_all = "snake_case")]
pub enum ProvenanceEvidence {
    CompletePositionMap,
    /// Stable structured record IDs bound to this snapshot, not free-form text.
    StructuredGenerationRecords {
        record_ids: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ProvenanceStatus {
    Verified { evidence: ProvenanceEvidence },
    Unknown { reason: ProvenanceUnknownReason },
}

/// One sidecar row for the current redacted snapshot. Validate both revision and
/// hash plus every position before use. Unknown status is never promoted by text
/// equality, absence of fork metadata, a later append, or migration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotProvenance {
    pub run_id: String,
    pub snapshot_revision: u64,
    pub snapshot_sha256: String,
    pub format_version: u32,
    pub status: ProvenanceStatus,
    pub map: ProvenanceMap,
}

/// Runtime supplies positions; storage assigns revision/hash after redaction.
/// Restore, fork/safe-prefix and System insertion must transform raw indices and
/// maps together. Raw-preserving compaction/model switches only add checkpoints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotWrite {
    pub record: RunContextRecord,
    pub expected_snapshot_revision: Option<u64>,
    pub binding: Option<ThreadRunRow>,
    pub status: ProvenanceStatus,
    pub map: ProvenanceMap,
}

/// Read in ONE transaction, including the latest ledger entry (empty = clear).
/// Missing/unverified provenance is representable for legacy data and is blocked
/// by extraction, not silently treated as native.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotBundle {
    pub record: RunContextRecord,
    pub snapshot_revision: u64,
    pub source_revision: u64,
    pub provenance: Option<SnapshotProvenance>,
    pub latest_note: Option<RunLedgerEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotReceipt {
    pub snapshot_revision: u64,
    pub source_revision: u64,
    pub snapshot_sha256: String,
}

/// The input identity comes from the same read transaction as the JSON and map.
/// `snapshot_sha256` is computed over the exact saved redacted JSON bytes using
/// `snapshot_sha256`, not a reserialization of a parsed legacy value.
#[derive(Debug, Clone, Copy)]
pub struct IndexInput<'a> {
    pub run_id: &'a str,
    pub snapshot_revision: u64,
    pub source_revision: u64,
    pub snapshot_sha256: &'a str,
    pub messages: &'a serde_json::Value,
    pub checkpoint_count: usize,
    pub provenance: Option<&'a SnapshotProvenance>,
    pub latest_note: Option<&'a RunLedgerEntry>,
}

/// Typed locators prevent note seqs from being interpreted as message ordinals.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "source_kind", rename_all = "snake_case")]
pub enum BlockLocator {
    Message { ordinal: usize, block_index: usize },
    WorkingNote { note_seq: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuardedReference {
    pub version: u32,
    pub run_id: String,
    pub source_revision: u64,
    #[serde(flatten)]
    pub source: GuardedSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source_kind", rename_all = "snake_case")]
pub enum GuardedSource {
    Message {
        snapshot_revision: u64,
        ordinal: usize,
        block_index: usize,
    },
    WorkingNote {
        note_seq: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexBlockKind {
    Text,
    WorkingNote,
    ToolUse,
    ToolResult,
    ToolError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexRole {
    User,
    Assistant,
    WorkingNote,
}

/// `text` and tool metadata are redacted; hash is SHA-256 of final UTF-8 text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexBlock {
    pub locator: BlockLocator,
    pub role: IndexRole,
    pub kind: IndexBlockKind,
    pub tool_name: Option<String>,
    pub text: String,
    pub truncated: bool,
    pub hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedIndexInput {
    pub run_id: String,
    pub snapshot_revision: u64,
    pub source_revision: u64,
    pub extractor_version: u32,
    pub blocks: Vec<IndexBlock>,
    /// Exact post-redaction/post-truncation UTF-8 byte count; NOT a physical or
    /// logical charged-byte estimate. Measurement chooses accounting coefficients.
    pub text_bytes: u64,
}

/// No parser diagnostics or source strings: errors must be safe to log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", content = "detail", rename_all = "snake_case")]
pub enum ExtractionError {
    InvalidJson,
    ProvenanceUnknown(ProvenanceUnknownReason),
    NoteRunMismatch,
}

impl std::fmt::Display for ExtractionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for ExtractionError {}
