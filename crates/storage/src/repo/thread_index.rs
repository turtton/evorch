//! Phase B persistence (ADR 0041 R1/R2/M3): atomic snapshot bundles and the
//! conversation index state machine. Every write to `thread_index_runs`,
//! `conversation_blocks` and the provenance sidecar goes through this module.

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::repo::{run_context, run_ledger};
use crate::thread_index::{
    BlockLocator, CommitRejection, ExtractedIndexInput, ExtractionError, GUARDED_REFERENCE_VERSION,
    GuardedReference, GuardedResolution, GuardedSource, IndexAccounting, IndexBlock,
    IndexBlockKind, IndexBlockedReason, IndexCommitGuard, IndexCommitResult, IndexEvent,
    IndexEvictionReason, IndexFailure, IndexOwnerKind, IndexRole, IndexRunStatus,
    IndexSourceHeader, IndexState, IndexTarget, NoteReceipt, PROVENANCE_FORMAT_VERSION,
    ProvenanceMap, ProvenanceStatus, ProvenanceUnknownReason, RequestGapReason, SnapshotApiError,
    SnapshotBundle, SnapshotProvenance, SnapshotReceipt, SnapshotRepository, SnapshotTransaction,
    SnapshotWrite, ThreadIndexRepository, snapshot_sha256, validate_saved_provenance,
};
use crate::threads::ThreadRunRow;

// Until the writer job drives the index (ADR 0041 順序5), only tests call it.
mod dependencies;
#[allow(dead_code)]
mod index;
use crate::{RunContextReceipt, RunContextRecord, RunLedgerEntry, StorageError};
pub(crate) use dependencies::LegacyDependencies;
#[allow(unused_imports)]
pub(crate) use index::{DiscoveryBatch, MAX_ATTEMPTS, SqliteThreadIndex, retry_delay_ns};

/// What one R2 cell does. `Keep` also preserves attempts, retry time and reasons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IndexTransition {
    Keep,
    /// Evicted rows record the new source identity but stay evicted (B-3).
    SourceInfoOnly,
    /// Tombstones never revive; a later source is only flagged.
    MarkSourceAfterTombstone,
    /// Back to pending with a new work generation.
    Pending {
        reset_attempts: bool,
    },
    UpdateAccountingVersion,
    /// Recompute charges from stored totals without re-extracting text.
    Reaccount,
    Indexed,
    /// A delayed or misdirected job result is dropped.
    Discard,
    CountFailure,
    Block,
    Evict,
    Delete,
}

/// The ADR 0041 R2 matrix (event × current state), the single source of truth
/// for the SQL below and for the later job/request code.
pub(crate) fn index_transition(state: IndexState, event: IndexEvent) -> IndexTransition {
    use IndexState::*;
    use IndexTransition as T;
    let live = matches!(state, Pending | Indexed | RetryWait | Blocked);
    match event {
        IndexEvent::DiscoverSameRevision
        | IndexEvent::SettingsApplied
        | IndexEvent::CapacityPaused => T::Keep,
        IndexEvent::SourceUpdated => match state {
            Evicted => T::SourceInfoOnly,
            Deleted => T::MarkSourceAfterTombstone,
            _ => T::Pending {
                reset_attempts: true,
            },
        },
        IndexEvent::Reindex { include_evicted } => match state {
            Evicted if include_evicted => T::Pending {
                reset_attempts: true,
            },
            _ if live => T::Pending {
                reset_attempts: true,
            },
            _ => T::Keep,
        },
        IndexEvent::ExtractorChanged if live => T::Pending {
            reset_attempts: true,
        },
        IndexEvent::ExtractorChanged => T::Keep,
        IndexEvent::AccountingChanged => match state {
            Indexed => T::Reaccount,
            Deleted => T::Keep,
            _ => T::UpdateAccountingVersion,
        },
        IndexEvent::RetryDue if state == RetryWait => T::Pending {
            reset_attempts: false,
        },
        IndexEvent::RetryDue => T::Keep,
        IndexEvent::Commit if state == Pending => T::Indexed,
        IndexEvent::TransientFailure if state == Pending => T::CountFailure,
        IndexEvent::Blocked if state == Pending => T::Block,
        IndexEvent::Commit | IndexEvent::TransientFailure | IndexEvent::Blocked => T::Discard,
        IndexEvent::Evict if live => T::Evict,
        IndexEvent::Evict => T::Keep,
        IndexEvent::DeleteSource if state == Deleted => T::Keep,
        IndexEvent::DeleteSource => T::Delete,
    }
}

/// Runs `body` in a savepoint: it composes with a caller's transaction, and
/// outside one it is a deferred transaction committed on success.
fn atomic<T, E: From<rusqlite::Error>>(
    conn: &Connection,
    body: impl FnOnce() -> Result<T, E>,
) -> Result<T, E> {
    let outermost = conn.is_autocommit();
    conn.execute_batch("SAVEPOINT thread_index")?;
    let result = body();
    let released = match &result {
        Ok(_) => conn.execute_batch("RELEASE thread_index"),
        Err(_) => Ok(()),
    };
    if result.is_err() || released.is_err() {
        let _ = conn.execute_batch("ROLLBACK TO thread_index; RELEASE thread_index");
        if outermost && !conn.is_autocommit() {
            let _ = conn.execute_batch("ROLLBACK");
        }
    }
    released?;
    result
}

pub(crate) fn into_storage_error(error: SnapshotApiError) -> StorageError {
    match error {
        SnapshotApiError::Storage(error) => error,
        SnapshotApiError::RevisionConflict => StorageError::StaleMutation,
        SnapshotApiError::InvalidSnapshot => {
            StorageError::Serialization("run context snapshot is not valid JSON".into())
        }
        SnapshotApiError::InvalidProvenance(error) => {
            StorageError::Serialization(format!("invalid snapshot provenance: {error}"))
        }
    }
}

fn serde_name(value: &impl Serialize) -> Result<String, StorageError> {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(name)) => Ok(name),
        _ => Err(StorageError::Serialization(
            "enum must serialize to a name".into(),
        )),
    }
}

fn to_json(value: &impl Serialize) -> Result<String, StorageError> {
    serde_json::to_string(value).map_err(|error| StorageError::Serialization(error.to_string()))
}

fn parse_state(name: &str) -> Result<IndexState, StorageError> {
    Ok(match name {
        "pending" => IndexState::Pending,
        "indexed" => IndexState::Indexed,
        "retry_wait" => IndexState::RetryWait,
        "blocked" => IndexState::Blocked,
        "evicted" => IndexState::Evicted,
        "deleted" => IndexState::Deleted,
        _ => return Err(StorageError::OutOfRange("thread index state")),
    })
}

#[allow(
    dead_code,
    reason = "fields beyond state/revision are read by the index job API"
)]
struct RunRow {
    state: IndexState,
    source_revision: u64,
    work_generation: u64,
    source_present: bool,
    extractor_version: u32,
    accounting_version: u32,
    attempt_count: u32,
}

fn run_row(conn: &Connection, run_id: &str) -> Result<Option<RunRow>, StorageError> {
    let row = conn
        .query_row(
            "SELECT state, source_revision, work_generation, source_present, extractor_version, \
             accounting_version, attempt_count FROM thread_index_runs WHERE run_id = ?1",
            [run_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .optional()?;
    row.map(
        |(state, source_revision, work_generation, present, ev, av, attempts)| {
            Ok(RunRow {
                state: parse_state(&state)?,
                source_revision,
                work_generation,
                source_present: present,
                extractor_version: ev,
                accounting_version: av,
                attempt_count: attempts,
            })
        },
    )
    .transpose()
}

fn applied_versions(conn: &Connection) -> Result<(u32, u32), StorageError> {
    Ok(conn.query_row(
        "SELECT extractor_version, accounting_version FROM thread_index_state WHERE id = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?)
}

/// Earliest known time of a run that has no index row yet; read before the
/// canonical write so a legacy snapshot keeps its older time.
fn retention_floor(conn: &Connection, run_id: &str) -> Result<Option<i64>, StorageError> {
    Ok(conn.query_row(
        "SELECT MIN(at) FROM (SELECT updated_at_ns AS at FROM run_contexts WHERE run_id = ?1 \
         UNION ALL SELECT created_at_ns FROM run_ledger WHERE run_id = ?1)",
        [run_id],
        |row| row.get(0),
    )?)
}

fn insert_pending(conn: &Connection, header: &IndexSourceHeader) -> Result<(), StorageError> {
    conn.execute(
        "INSERT INTO thread_index_runs(run_id, state, source_revision, snapshot_revision, \
         source_context_sha256, source_note_seq, source_provenance_version, extractor_version, \
         accounting_version, source_updated_at_ns, retention_at_ns, work_generation) \
         VALUES (?1, 'pending', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 1)",
        params![
            header.target.run_id,
            header.target.source_revision,
            header.snapshot_revision,
            header.context_sha256,
            header.note_seq,
            header.provenance_version,
            header.target.extractor_version,
            header.target.accounting_version,
            header.source_updated_at_ns,
            header.retention_at_ns,
        ],
    )?;
    Ok(())
}

/// Source identity of one canonical change; `None` keeps the stored value.
#[derive(Default)]
struct SourceValues<'a> {
    /// `None` advances the revision by one.
    source_revision: Option<u64>,
    snapshot_revision: Option<u64>,
    sha256: Option<&'a str>,
    provenance_version: Option<u32>,
    note_seq: Option<u64>,
    updated_at_ns: i64,
}

fn update_source(
    conn: &Connection,
    run_id: &str,
    values: &SourceValues<'_>,
    to_pending: bool,
) -> Result<u64, StorageError> {
    let pending = if to_pending {
        ", state = 'pending', attempt_count = 0, next_attempt_at_ns = NULL, \
         blocked_reason = NULL, last_error_code = NULL, work_generation = work_generation + 1"
    } else {
        ""
    };
    Ok(conn.query_row(
        &format!(
            "UPDATE thread_index_runs SET \
             source_revision = COALESCE(?2, source_revision + 1), \
             snapshot_revision = COALESCE(?3, snapshot_revision), \
             source_context_sha256 = COALESCE(?4, source_context_sha256), \
             source_provenance_version = COALESCE(?5, source_provenance_version), \
             source_note_seq = COALESCE(?6, source_note_seq), \
             source_updated_at_ns = ?7, source_present = 1{pending} \
             WHERE run_id = ?1 RETURNING source_revision"
        ),
        params![
            run_id,
            values.source_revision,
            values.snapshot_revision,
            values.sha256,
            values.provenance_version,
            values.note_seq,
            values.updated_at_ns,
        ],
        |row| row.get(0),
    )?)
}

/// R2 "new snapshot / note / provenance revision", in the canonical write's
/// transaction. Returns the source revision readers will observe.
fn record_source_change(
    conn: &Connection,
    run_id: &str,
    values: SourceValues<'_>,
    retention_floor: Option<i64>,
) -> Result<u64, StorageError> {
    let Some(row) = run_row(conn, run_id)? else {
        let (extractor_version, accounting_version) = applied_versions(conn)?;
        let (snapshot_revision, sha256, provenance_version) = match values.snapshot_revision {
            Some(revision) => (
                revision,
                values.sha256.unwrap_or_default().to_owned(),
                values.provenance_version,
            ),
            None => conn
                .query_row(
                    "SELECT snapshot_revision, snapshot_sha256, format_version \
                     FROM run_context_provenance WHERE run_id = ?1",
                    [run_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?
                .unwrap_or_default(),
        };
        let note_seq = match values.note_seq {
            Some(seq) => Some(seq),
            None => conn.query_row(
                "SELECT MAX(seq) FROM run_ledger WHERE run_id = ?1",
                [run_id],
                |row| row.get(0),
            )?,
        };
        let retention_at_ns = [
            retention_floor,
            crate::thread_index::run_id_created_at_ns(run_id),
        ]
        .into_iter()
        .flatten()
        .fold(values.updated_at_ns, i64::min);
        insert_pending(
            conn,
            &IndexSourceHeader {
                target: IndexTarget {
                    run_id: run_id.to_owned(),
                    source_revision: 1,
                    extractor_version,
                    accounting_version,
                },
                snapshot_revision,
                context_sha256: sha256,
                note_seq,
                provenance_version,
                source_updated_at_ns: values.updated_at_ns,
                retention_at_ns,
                source_present: true,
            },
        )?;
        return Ok(1);
    };
    match index_transition(row.state, IndexEvent::SourceUpdated) {
        IndexTransition::MarkSourceAfterTombstone => {
            conn.execute(
                "UPDATE thread_index_runs SET source_after_tombstone = 1 WHERE run_id = ?1",
                [run_id],
            )?;
            Ok(row.source_revision)
        }
        IndexTransition::SourceInfoOnly => update_source(conn, run_id, &values, false),
        IndexTransition::Pending { .. } => update_source(conn, run_id, &values, true),
        _ => Err(StorageError::OutOfRange("thread index source transition")),
    }
}

enum ProvenanceInput<'a> {
    Supplied {
        status: &'a ProvenanceStatus,
        map: &'a ProvenanceMap,
    },
    /// Writers that do not produce positions yet; never treated as verified.
    Legacy,
}

/// Snapshot row, binding, sidecar, source revision and R2 state in the caller's
/// savepoint. Nothing is visible unless every step succeeds.
fn stage(
    conn: &Connection,
    record: &RunContextRecord,
    expected_snapshot_revision: Option<u64>,
    binding: Option<&ThreadRunRow>,
    provenance: ProvenanceInput<'_>,
) -> Result<(SnapshotReceipt, RunContextReceipt), SnapshotApiError> {
    atomic(conn, || {
        let current: u64 = conn
            .query_row(
                "SELECT snapshot_revision FROM run_context_provenance WHERE run_id = ?1",
                [&record.run_id],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0);
        if expected_snapshot_revision.is_some_and(|expected| expected != current) {
            return Err(SnapshotApiError::RevisionConflict);
        }
        let redacted = run_context::redact(record)?.ok_or(SnapshotApiError::InvalidSnapshot)?;
        let snapshot_revision = current
            .checked_add(1)
            .ok_or(StorageError::OutOfRange("snapshot revision"))?;
        let floor = match run_row(conn, &record.run_id)? {
            Some(_) => None,
            None => retention_floor(conn, &record.run_id)?,
        };
        run_context::write(conn, record, &redacted, snapshot_revision)?;
        let sha256 = snapshot_sha256(&redacted.messages_json, &redacted.checkpoints_json);
        let mut sidecar = SnapshotProvenance {
            run_id: record.run_id.clone(),
            snapshot_revision,
            snapshot_sha256: sha256.clone(),
            format_version: PROVENANCE_FORMAT_VERSION,
            status: ProvenanceStatus::Unknown {
                reason: ProvenanceUnknownReason::LegacyUnproven,
            },
            map: ProvenanceMap {
                version: PROVENANCE_FORMAT_VERSION,
                raw_message_count: redacted.messages_end(),
                checkpoint_count: redacted.checkpoint_count,
                messages: Vec::new(),
            },
        };
        if let ProvenanceInput::Supplied { status, map } = provenance {
            sidecar.status = status.clone();
            sidecar.map = map.clone();
            if matches!(status, ProvenanceStatus::Verified { .. })
                && let Err(error) = validate_saved_provenance(
                    &sidecar,
                    &redacted.messages,
                    redacted.checkpoint_count,
                )
            {
                // The canonical snapshot wins: keep it, but never as verified.
                let reason = match error {
                    ExtractionError::ProvenanceUnknown(reason) => reason,
                    _ => ProvenanceUnknownReason::IncompleteMap,
                };
                tracing::warn!(run_id = %record.run_id, ?reason, "snapshot provenance downgraded to unknown");
                sidecar.status = ProvenanceStatus::Unknown { reason };
            }
        }
        write_sidecar(conn, &sidecar)?;
        match binding {
            Some(binding) if binding.run_id != record.run_id => {
                return Err(SnapshotApiError::InvalidSnapshot);
            }
            Some(binding) => {
                conn.execute(
                    "INSERT INTO thread_runs (run_id, thread_id, root_run_id, depth) \
                     VALUES (?1, ?2, ?3, ?4) ON CONFLICT(run_id) DO UPDATE SET \
                     thread_id = excluded.thread_id, root_run_id = excluded.root_run_id, \
                     depth = excluded.depth",
                    params![
                        binding.run_id,
                        binding.thread_id,
                        binding.root_run_id,
                        binding.depth
                    ],
                )?;
            }
            None => crate::repo::threads::bind_saved_run(conn, record)?,
        }
        let source_revision = record_source_change(
            conn,
            &record.run_id,
            SourceValues {
                snapshot_revision: Some(snapshot_revision),
                sha256: Some(&sha256),
                provenance_version: Some(PROVENANCE_FORMAT_VERSION),
                updated_at_ns: record.updated_at_ns,
                ..SourceValues::default()
            },
            floor,
        )?;
        Ok((
            SnapshotReceipt {
                snapshot_revision,
                source_revision,
                snapshot_sha256: sha256,
            },
            RunContextReceipt {
                updated_at_ns: record.updated_at_ns,
                messages_end: redacted.messages_end(),
                sha256: format!("{:x}", Sha256::digest(redacted.messages_json.as_bytes())),
            },
        ))
    })
}

pub(crate) fn stage_legacy_snapshot(
    conn: &Connection,
    record: &RunContextRecord,
) -> Result<(SnapshotReceipt, RunContextReceipt), SnapshotApiError> {
    stage(conn, record, None, None, ProvenanceInput::Legacy)
}

pub(crate) fn stage_snapshot(
    conn: &Connection,
    write: &SnapshotWrite,
) -> Result<SnapshotReceipt, SnapshotApiError> {
    stage(
        conn,
        &write.record,
        write.expected_snapshot_revision,
        write.binding.as_ref(),
        ProvenanceInput::Supplied {
            status: &write.status,
            map: &write.map,
        },
    )
    .map(|(receipt, _)| receipt)
}

fn write_sidecar(conn: &Connection, sidecar: &SnapshotProvenance) -> Result<(), StorageError> {
    let (status, reason, evidence) = match &sidecar.status {
        ProvenanceStatus::Verified { evidence } => ("verified", None, Some(to_json(evidence)?)),
        ProvenanceStatus::Unknown { reason } => ("unknown", Some(serde_name(reason)?), None),
    };
    conn.execute(
        "INSERT INTO run_context_provenance(run_id, snapshot_revision, snapshot_sha256, \
         format_version, status, reason_code, evidence_json, map_json) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) ON CONFLICT(run_id) DO UPDATE SET \
         snapshot_revision = excluded.snapshot_revision, \
         snapshot_sha256 = excluded.snapshot_sha256, format_version = excluded.format_version, \
         status = excluded.status, reason_code = excluded.reason_code, \
         evidence_json = excluded.evidence_json, map_json = excluded.map_json",
        params![
            sidecar.run_id,
            sidecar.snapshot_revision,
            sidecar.snapshot_sha256,
            sidecar.format_version,
            status,
            reason,
            evidence,
            to_json(&sidecar.map)?,
        ],
    )?;
    Ok(())
}

pub(crate) fn read_sidecar(
    conn: &Connection,
    run_id: &str,
) -> Result<Option<SnapshotProvenance>, StorageError> {
    type Row = (
        u64,
        String,
        u32,
        String,
        Option<String>,
        Option<String>,
        String,
    );
    let row: Option<Row> = conn
        .query_row(
            "SELECT snapshot_revision, snapshot_sha256, format_version, status, reason_code, \
             evidence_json, map_json FROM run_context_provenance WHERE run_id = ?1",
            [run_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .optional()?;
    let Some((snapshot_revision, snapshot_sha256, format_version, status, reason, evidence, map)) =
        row
    else {
        return Ok(None);
    };
    // Unreadable sidecars are kept representable and can only block indexing.
    let unsupported = ProvenanceStatus::Unknown {
        reason: ProvenanceUnknownReason::UnsupportedVersion,
    };
    let status = match (status.as_str(), reason, evidence) {
        ("verified", _, Some(evidence)) => serde_json::from_str(&evidence)
            .map(|evidence| ProvenanceStatus::Verified { evidence })
            .unwrap_or(unsupported.clone()),
        ("unknown", Some(reason), _) => serde_json::from_value(serde_json::Value::String(reason))
            .map(|reason| ProvenanceStatus::Unknown { reason })
            .unwrap_or(unsupported.clone()),
        _ => unsupported.clone(),
    };
    let (status, map) = match serde_json::from_str::<ProvenanceMap>(&map) {
        Ok(map) => (status, map),
        Err(_) => (
            unsupported,
            ProvenanceMap {
                version: 0,
                raw_message_count: 0,
                checkpoint_count: 0,
                messages: Vec::new(),
            },
        ),
    };
    Ok(Some(SnapshotProvenance {
        run_id: run_id.to_owned(),
        snapshot_revision,
        snapshot_sha256,
        format_version,
        status,
        map,
    }))
}

fn current_source_revision(conn: &Connection, run_id: &str) -> Result<u64, StorageError> {
    Ok(conn
        .query_row(
            "SELECT source_revision FROM thread_index_runs WHERE run_id = ?1",
            [run_id],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(0))
}

fn load_bundle(conn: &Connection, run_id: &str) -> Result<Option<SnapshotBundle>, StorageError> {
    let row = conn
        .query_row(
            &format!(
                "SELECT {}, snapshot_revision FROM run_contexts WHERE run_id = ?1",
                run_context::COLUMNS
            ),
            [run_id],
            |row| Ok((run_context::from_row(row)?, row.get::<_, u64>(10)?)),
        )
        .optional()?;
    let Some((record, snapshot_revision)) = row else {
        return Ok(None);
    };
    let mut provenance = read_sidecar(conn, run_id)?;
    if provenance.is_none() && snapshot_revision == 0 {
        provenance = Some(
            crate::thread_index::derive_legacy_provenance(conn, &record)?.unwrap_or_else(|_| {
                SnapshotProvenance {
                    run_id: run_id.to_owned(),
                    snapshot_revision,
                    snapshot_sha256: snapshot_sha256(
                        &record.messages_json,
                        &record.checkpoints_json,
                    ),
                    format_version: PROVENANCE_FORMAT_VERSION,
                    status: ProvenanceStatus::Unknown {
                        reason: ProvenanceUnknownReason::LegacyUnproven,
                    },
                    map: ProvenanceMap {
                        version: PROVENANCE_FORMAT_VERSION,
                        raw_message_count: 0,
                        checkpoint_count: 0,
                        messages: Vec::new(),
                    },
                }
            }),
        );
    }
    Ok(Some(SnapshotBundle {
        record,
        snapshot_revision,
        source_revision: current_source_revision(conn, run_id)?,
        provenance,
        latest_note: run_ledger::latest(conn, run_id)?,
    }))
}

/// Note append (an empty body clears) plus R2 source change, one transaction.
pub(crate) fn append_note(
    conn: &Connection,
    run_id: &str,
    body: &str,
    created_at_ns: i64,
) -> Result<NoteReceipt, StorageError> {
    atomic(conn, || {
        let floor = match run_row(conn, run_id)? {
            Some(_) => None,
            None => retention_floor(conn, run_id)?,
        };
        let seq = run_ledger::append_at(conn, run_id, body, created_at_ns)?;
        let source_revision = record_source_change(
            conn,
            run_id,
            SourceValues {
                note_seq: Some(seq),
                updated_at_ns: created_at_ns,
                ..SourceValues::default()
            },
            floor,
        )?;
        Ok(NoteReceipt {
            note: RunLedgerEntry {
                seq,
                run_id: run_id.to_owned(),
                body: body.to_owned(),
                created_at_ns,
            },
            source_revision,
        })
    })
}

fn message_block_exists(messages_json: &str, ordinal: usize, block_index: usize) -> bool {
    let Ok(serde_json::Value::Array(messages)) = serde_json::from_str(messages_json) else {
        return false;
    };
    messages
        .iter()
        .filter(|message| message["role"] != "system")
        .nth(ordinal)
        .and_then(|message| message["content"].as_array())
        .is_some_and(|content| block_index < content.len())
}

fn resolve(
    conn: &Connection,
    reference: &GuardedReference,
) -> Result<GuardedResolution, StorageError> {
    if reference.version != GUARDED_REFERENCE_VERSION {
        return Ok(GuardedResolution::UnsupportedVersion);
    }
    atomic(conn, || {
        if run_row(conn, &reference.run_id)?
            .is_some_and(|row| row.state == IndexState::Deleted || !row.source_present)
        {
            return Ok(GuardedResolution::SourceDeleted);
        }
        if current_source_revision(conn, &reference.run_id)? != reference.source_revision {
            return Ok(GuardedResolution::StaleReference);
        }
        match reference.source {
            GuardedSource::Message {
                snapshot_revision,
                ordinal,
                block_index,
            } => {
                let Some(bundle) = load_bundle(conn, &reference.run_id)? else {
                    return Ok(GuardedResolution::StaleReference);
                };
                if bundle.snapshot_revision != snapshot_revision
                    || !message_block_exists(&bundle.record.messages_json, ordinal, block_index)
                {
                    return Ok(GuardedResolution::StaleReference);
                }
                Ok(GuardedResolution::Message {
                    bundle: Box::new(bundle),
                    ordinal,
                    block_index,
                })
            }
            GuardedSource::WorkingNote { note_seq } => {
                Ok(match run_ledger::latest(conn, &reference.run_id)? {
                    Some(note) if note.seq == note_seq && !note.body.is_empty() => {
                        GuardedResolution::WorkingNote(note)
                    }
                    _ => GuardedResolution::StaleReference,
                })
            }
        }
    })
}

/// `SnapshotRepository` over a connection; each call is its own transaction
/// (or a savepoint of the caller's).
pub(crate) struct SqliteSnapshotStore<'c> {
    conn: &'c Connection,
}

impl<'c> SqliteSnapshotStore<'c> {
    pub(crate) fn new(conn: &'c Connection) -> Self {
        Self { conn }
    }
}

impl SnapshotRepository for SqliteSnapshotStore<'_> {
    fn save_snapshot_bundle(
        &mut self,
        write: &SnapshotWrite,
    ) -> Result<SnapshotReceipt, SnapshotApiError> {
        stage_snapshot(self.conn, write)
    }

    fn load_snapshot_bundle(
        &self,
        run_id: &str,
    ) -> Result<Option<SnapshotBundle>, SnapshotApiError> {
        Ok(atomic(self.conn, || load_bundle(self.conn, run_id))?)
    }

    fn append_indexed_note(
        &mut self,
        run_id: &str,
        body: &str,
        created_at_ns: i64,
    ) -> Result<NoteReceipt, SnapshotApiError> {
        Ok(append_note(self.conn, run_id, body, created_at_ns)?)
    }

    fn resolve_guarded_reference(
        &self,
        reference: &GuardedReference,
    ) -> Result<GuardedResolution, SnapshotApiError> {
        Ok(resolve(self.conn, reference)?)
    }
}

/// Stages a bundle inside an evidence/reference-consumption transaction.
pub(crate) struct StagedSnapshot<'c>(pub(crate) &'c Connection);

impl SnapshotTransaction for StagedSnapshot<'_> {
    fn stage_snapshot_bundle(
        &mut self,
        write: &SnapshotWrite,
    ) -> Result<SnapshotReceipt, SnapshotApiError> {
        stage_snapshot(self.0, write)
    }
}

pub(crate) fn index_run_status(
    conn: &Connection,
    run_id: &str,
) -> Result<Option<IndexRunStatus>, StorageError> {
    let row = conn
        .query_row(
            "SELECT source_revision, extractor_version, accounting_version, snapshot_revision, \
             source_context_sha256, source_note_seq, source_provenance_version, \
             source_updated_at_ns, retention_at_ns, source_present, state, indexed_revision, \
             work_generation, indexed_at_ns, text_bytes, block_count, charged_bytes, priority, \
             attempt_count, next_attempt_at_ns, blocked_reason, last_error_code \
             FROM thread_index_runs WHERE run_id = ?1",
            [run_id],
            |row| {
                let source = IndexSourceHeader {
                    target: IndexTarget {
                        run_id: run_id.to_owned(),
                        source_revision: row.get(0)?,
                        extractor_version: row.get(1)?,
                        accounting_version: row.get(2)?,
                    },
                    snapshot_revision: row.get(3)?,
                    context_sha256: row.get(4)?,
                    note_seq: row.get(5)?,
                    provenance_version: row.get(6)?,
                    source_updated_at_ns: row.get(7)?,
                    retention_at_ns: row.get(8)?,
                    source_present: row.get(9)?,
                };
                Ok((
                    source,
                    row.get::<_, String>(10)?,
                    row.get(11)?,
                    row.get(12)?,
                    row.get(13)?,
                    (row.get(14)?, row.get(15)?, row.get(16)?, row.get(17)?),
                    (row.get(18)?, row.get(19)?),
                    (row.get::<_, Option<String>>(20)?, row.get(21)?),
                ))
            },
        )
        .optional()?;
    let Some((source, state, indexed_revision, work_generation, indexed_at_ns, totals, retry, why)) =
        row
    else {
        return Ok(None);
    };
    let (text_bytes, block_count, charged_bytes, priority) = totals;
    let (attempt_count, next_attempt_at_ns) = retry;
    let (blocked_reason, last_error_code) = why;
    Ok(Some(IndexRunStatus {
        source,
        state: parse_state(&state)?,
        indexed_revision,
        work_generation,
        indexed_at_ns,
        text_bytes,
        block_count,
        charged_bytes,
        priority,
        attempt_count,
        next_attempt_at_ns,
        blocked_reason: blocked_reason
            .map(|reason| {
                serde_json::from_str(&reason)
                    .map_err(|_| StorageError::OutOfRange("thread index blocked reason"))
            })
            .transpose()?,
        last_error_code,
    }))
}
