//! `ThreadIndexRepository`: discovery, guarded commits/failures, eviction and
//! tombstones. All R2 state SQL for index work lives here and in the parent.

use std::collections::HashSet;

use rusqlite::{Connection, params};

use super::*;
use crate::thread_index::{PurgeProgress, PurgeTarget};

/// Failures of one source revision before it becomes `blocked`.
pub(crate) const MAX_ATTEMPTS: u32 = 5;
const RETRY_BASE_NS: i64 = 1_000_000_000;
const RETRY_MAX_NS: i64 = 300 * RETRY_BASE_NS;
const NOTE_SOURCE_KEY: &str = "working_note";

/// `1s × 2^(attempt-1)`, capped at 5 minutes.
pub(crate) fn retry_delay_ns(attempt: u32) -> i64 {
    let exponent = attempt.saturating_sub(1).min(30);
    RETRY_BASE_NS
        .saturating_mul(1_i64 << exponent)
        .min(RETRY_MAX_NS)
}

fn eviction_name(reason: IndexEvictionReason) -> &'static str {
    match reason {
        IndexEvictionReason::LogicalBudget => "logical_budget",
        IndexEvictionReason::Oversized => "oversized",
        IndexEvictionReason::PhysicalReclaim => "physical_reclaim",
        IndexEvictionReason::Archived => "archived",
    }
}

fn owner_kind_name(kind: IndexOwnerKind) -> &'static str {
    match kind {
        IndexOwnerKind::Gui => "gui",
        IndexOwnerKind::Cli => "cli",
    }
}

/// Stable, secret-free codes for `last_error_code`.
fn blocked_code(reason: IndexBlockedReason) -> &'static str {
    match reason {
        IndexBlockedReason::AttemptsExhausted => "attempts_exhausted",
        IndexBlockedReason::Extraction(ExtractionError::InvalidJson) => "invalid_json",
        IndexBlockedReason::Extraction(ExtractionError::ProvenanceUnknown(_)) => {
            "provenance_unknown"
        }
        IndexBlockedReason::Extraction(ExtractionError::NoteRunMismatch) => "note_run_mismatch",
    }
}

/// Discovery batch outcome; `complete` resets the resume position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DiscoveryBatch {
    pub discovered: usize,
    pub complete: bool,
}

/// `ThreadIndexRepository` over the writer connection, plus the job-side
/// queries of the same state machine.
pub(crate) struct SqliteThreadIndex<'c> {
    conn: &'c Connection,
}

impl<'c> SqliteThreadIndex<'c> {
    pub(crate) fn new(conn: &'c Connection) -> Self {
        Self { conn }
    }

    /// Header-only keyset scan, newest first, of canonical runs that have no
    /// index row yet; legacy snapshots carry revision 0 and no sidecar.
    pub(crate) fn discover_batch(
        &mut self,
        limit: usize,
        now_ns: i64,
    ) -> Result<DiscoveryBatch, StorageError> {
        let conn = self.conn;
        atomic(conn, || {
            let (cursor_at, cursor_run): (Option<i64>, Option<String>) = conn.query_row(
                "SELECT discovery_retention_at_ns, discovery_run_id FROM thread_index_state \
                 WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            let (extractor_version, accounting_version) = applied_versions(conn)?;
            let mut statement = conn.prepare(
                "WITH sources(run_id, at, seq) AS (\
                 SELECT run_id, updated_at_ns, NULL FROM run_contexts \
                 UNION ALL SELECT run_id, created_at_ns, seq FROM run_ledger) \
                 SELECT s.run_id, MIN(s.at), MAX(s.at), MAX(s.seq), p.snapshot_revision, \
                 p.snapshot_sha256, p.format_version \
                 FROM sources s LEFT JOIN run_context_provenance p ON p.run_id = s.run_id \
                 WHERE NOT EXISTS (SELECT 1 FROM thread_index_runs t WHERE t.run_id = s.run_id) \
                 GROUP BY s.run_id \
                 HAVING ?1 IS NULL OR (MIN(s.at), s.run_id) < (?1, ?2) \
                 ORDER BY MIN(s.at) DESC, s.run_id DESC LIMIT ?3",
            )?;
            let headers = statement
                .query_map(params![cursor_at, cursor_run, limit], |row| {
                    let run_id: String = row.get(0)?;
                    let scan_at: i64 = row.get(1)?;
                    let created = crate::thread_index::run_id_created_at_ns(&run_id);
                    let header = IndexSourceHeader {
                        target: IndexTarget {
                            run_id,
                            source_revision: 0,
                            extractor_version,
                            accounting_version,
                        },
                        retention_at_ns: created.map_or(scan_at, |created| created.min(scan_at)),
                        source_updated_at_ns: row.get(2)?,
                        note_seq: row.get(3)?,
                        snapshot_revision: row.get::<_, Option<u64>>(4)?.unwrap_or(0),
                        context_sha256: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
                        provenance_version: row.get(6)?,
                        source_present: true,
                    };
                    Ok((header, scan_at))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            for (header, _) in &headers {
                self.discover_source(header)?;
            }
            let complete = headers.len() < limit;
            // The resume position is the scan key, not the derived retention time.
            match headers.last().filter(|_| !complete) {
                Some((last, scan_at)) => conn.execute(
                    "UPDATE thread_index_state SET discovery_retention_at_ns = ?1, \
                     discovery_run_id = ?2 WHERE id = 1",
                    params![scan_at, last.target.run_id],
                )?,
                None => conn.execute(
                    "UPDATE thread_index_state SET discovery_retention_at_ns = NULL, \
                     discovery_run_id = NULL, discovery_completed_at_ns = ?1 WHERE id = 1",
                    [now_ns],
                )?,
            };
            Ok(DiscoveryBatch {
                discovered: headers.len(),
                complete,
            })
        })
    }

    /// R2 "retry 時刻到達": due retry rows return to pending, keeping attempts.
    pub(crate) fn promote_due_retries(&mut self, now_ns: i64) -> Result<usize, StorageError> {
        Ok(self.conn.execute(
            "UPDATE thread_index_runs SET state = 'pending', next_attempt_at_ns = NULL, \
             work_generation = work_generation + 1 \
             WHERE state = 'retry_wait' AND next_attempt_at_ns <= ?1",
            [now_ns],
        )?)
    }

    pub(crate) fn next_retry_at_ns(&self) -> Result<Option<i64>, StorageError> {
        Ok(self.conn.query_row(
            "SELECT MIN(next_attempt_at_ns) FROM thread_index_runs WHERE state = 'retry_wait'",
            [],
            |row| row.get(0),
        )?)
    }

    /// Runs holding index bytes, oldest first (B-4: creation time, then run_id).
    pub(crate) fn eviction_candidates(
        &self,
        limit: usize,
    ) -> Result<Vec<(String, u64)>, StorageError> {
        let mut statement = self.conn.prepare(
            "SELECT run_id, charged_bytes FROM thread_index_runs WHERE charged_bytes > 0 \
             ORDER BY retention_at_ns ASC, run_id ASC LIMIT ?1",
        )?;
        Ok(statement
            .query_map([limit], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_, _>>()?)
    }

    pub(crate) fn charged_bytes_total(&self) -> Result<u64, StorageError> {
        Ok(self.conn.query_row(
            "SELECT COALESCE(SUM(charged_bytes), 0) FROM thread_index_runs",
            [],
            |row| row.get(0),
        )?)
    }

    /// Materializes reader-derived legacy evidence without reading a snapshot
    /// on the writer. A source change or lease takeover rejects the result.
    pub(crate) fn commit_legacy_provenance(
        &mut self,
        guard: &IndexCommitGuard,
        sidecar: &SnapshotProvenance,
        dependencies: &LegacyDependencies,
        now_ns: i64,
    ) -> Result<IndexCommitResult, StorageError> {
        atomic(self.conn, || {
            if let Some(rejection) = self.check_guard(guard, now_ns)? {
                return Ok(IndexCommitResult::Discarded(rejection));
            }
            if !dependencies.unchanged(self.conn)? {
                return Ok(IndexCommitResult::Discarded(CommitRejection::SourceChanged));
            }
            let revision: Option<u64> = self
                .conn
                .query_row(
                    "SELECT snapshot_revision FROM run_contexts WHERE run_id = ?1",
                    [&guard.target.run_id],
                    |row| row.get(0),
                )
                .optional()?;
            if revision != Some(0)
                || sidecar.snapshot_revision != 0
                || sidecar.run_id != guard.target.run_id
                || read_sidecar(self.conn, &guard.target.run_id)?.is_some()
            {
                return Ok(IndexCommitResult::Discarded(CommitRejection::SourceChanged));
            }
            write_sidecar(self.conn, sidecar)?;
            // Evidence is a source change, not an in-place native promotion.
            // Its new generation must be re-extracted before indexing.
            record_source_change(
                self.conn,
                &guard.target.run_id,
                SourceValues {
                    snapshot_revision: Some(0),
                    sha256: Some(&sidecar.snapshot_sha256),
                    provenance_version: Some(sidecar.format_version),
                    updated_at_ns: now_ns,
                    ..SourceValues::default()
                },
                None,
            )?;
            Ok(IndexCommitResult::Committed)
        })
    }

    pub(crate) fn check_guard(
        &self,
        guard: &IndexCommitGuard,
        now_ns: i64,
    ) -> Result<Option<CommitRejection>, StorageError> {
        let conn = self.conn;
        let lease: (Option<String>, Option<String>, u64, Option<i64>) = conn.query_row(
            "SELECT owner_id, owner_kind, epoch, expires_at_ns FROM thread_index_lease WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        if lease.0.as_deref() != Some(guard.lease.owner.id.as_str())
            || lease.1.as_deref() != Some(owner_kind_name(guard.lease.owner.kind))
            || lease.2 != guard.lease.epoch
            || lease.3.is_none_or(|expires_at_ns| expires_at_ns <= now_ns)
        {
            return Ok(Some(CommitRejection::LeaseLost));
        }
        let target = &guard.target;
        let Some(row) = run_row(conn, &target.run_id)? else {
            return Ok(Some(CommitRejection::SourceDeleted));
        };
        let rejection = if row.state == IndexState::Deleted
            || !row.source_present
            || !guard.source_present
        {
            CommitRejection::SourceDeleted
        } else if row.source_revision != target.source_revision {
            CommitRejection::SourceChanged
        } else if row.work_generation != guard.work_generation {
            CommitRejection::GenerationChanged
        } else if row.state != IndexState::Pending || guard.expected_state != IndexState::Pending {
            CommitRejection::StateChanged
        } else if row.extractor_version != target.extractor_version
            || row.accounting_version != target.accounting_version
        {
            CommitRejection::VersionChanged
        } else {
            return Ok(guard
                .request
                .as_ref()
                .map(|request| self.request_active(request, guard))
                .transpose()?
                .and_then(|active| (!active).then_some(CommitRejection::RequestInactive)));
        };
        Ok(Some(rejection))
    }

    fn request_active(
        &self,
        request: &crate::thread_index::RequestGuard,
        guard: &IndexCommitGuard,
    ) -> Result<bool, StorageError> {
        if request.target != guard.target || request.work_generation != guard.work_generation {
            return Ok(false);
        }
        let row: Option<(IndexTarget, u64, String, String)> = self
            .conn
            .query_row(
                "SELECT t.run_id, t.source_revision, t.extractor_version, t.accounting_version, \
                 t.work_generation, t.state, r.state FROM thread_index_request_targets t \
                 JOIN thread_index_requests r ON r.request_id = t.request_id \
                 WHERE t.request_id = ?1 AND t.run_id = ?2",
                params![request.request_id, request.target.run_id],
                |row| {
                    Ok((
                        IndexTarget {
                            run_id: row.get(0)?,
                            source_revision: row.get(1)?,
                            extractor_version: row.get(2)?,
                            accounting_version: row.get(3)?,
                        },
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .optional()?;
        Ok(
            row.is_some_and(|(target, generation, target_state, request_state)| {
                target == request.target
                    && generation == request.work_generation
                    && target_state == "pending"
                    && request_state == "pending"
            }),
        )
    }

    fn close_request_target(
        &self,
        guard: &IndexCommitGuard,
        gap: Option<RequestGapReason>,
    ) -> Result<(), StorageError> {
        let Some(request) = &guard.request else {
            return Ok(());
        };
        let (state, reason) = match gap {
            Some(reason) => ("gap", Some(to_json(&reason)?)),
            None => ("succeeded", None),
        };
        self.conn.execute(
            "UPDATE thread_index_request_targets SET state = ?3, gap_reason = ?4 \
             WHERE request_id = ?1 AND run_id = ?2 AND state = 'pending'",
            params![request.request_id, request.target.run_id, state, reason],
        )?;
        Ok(())
    }

    fn block(
        &self,
        guard: &IndexCommitGuard,
        reason: IndexBlockedReason,
        attempt_count: u32,
    ) -> Result<(), StorageError> {
        self.conn.execute(
            "UPDATE thread_index_runs SET state = 'blocked', blocked_reason = ?2, \
             last_error_code = ?3, attempt_count = ?4, next_attempt_at_ns = NULL \
             WHERE run_id = ?1",
            params![
                guard.target.run_id,
                to_json(&reason)?,
                blocked_code(reason),
                attempt_count
            ],
        )?;
        self.close_request_target(guard, Some(RequestGapReason::Blocked(reason)))
    }

    /// Clears blocks (FTS follows via triggers) and totals for E / D.
    fn drop_index(&self, run_id: &str, assignments: &str) -> Result<(), StorageError> {
        self.conn.execute(
            "DELETE FROM conversation_blocks WHERE run_id = ?1",
            [run_id],
        )?;
        self.conn.execute(
            &format!(
                "UPDATE thread_index_runs SET {assignments}, indexed_revision = NULL, \
                 indexed_extractor_version = NULL, indexed_accounting_version = NULL, \
                 indexed_at_ns = NULL, text_bytes = 0, attribute_bytes = 0, block_count = 0, \
                 charged_bytes = 0, next_attempt_at_ns = NULL, blocked_reason = NULL, \
                 work_generation = work_generation + 1 WHERE run_id = ?1"
            ),
            [run_id],
        )?;
        Ok(())
    }
}

fn block_key(block: &IndexBlock) -> (&'static str, String) {
    match block.locator {
        BlockLocator::Message {
            ordinal,
            block_index,
        } => ("message", format!("{ordinal}:{block_index}")),
        BlockLocator::WorkingNote { .. } => ("working_note", NOTE_SOURCE_KEY.to_owned()),
    }
}

fn role_name(role: IndexRole) -> &'static str {
    match role {
        IndexRole::User => "user",
        IndexRole::Assistant => "assistant",
        IndexRole::WorkingNote => "working_note",
    }
}

fn kind_name(kind: IndexBlockKind) -> &'static str {
    match kind {
        IndexBlockKind::Text => "text",
        IndexBlockKind::WorkingNote => "working_note",
        IndexBlockKind::ToolUse => "tool_use",
        IndexBlockKind::ToolResult => "tool_result",
        IndexBlockKind::ToolError => "tool_error",
    }
}

/// Programming errors only; stale work is rejected by the guard instead.
fn validate_commit_input(
    guard: &IndexCommitGuard,
    input: &ExtractedIndexInput,
    accounting: &IndexAccounting,
) -> Result<(), StorageError> {
    let target = &guard.target;
    let text_bytes: u64 = input
        .blocks
        .iter()
        .map(|block| block.text.len() as u64)
        .sum();
    let mut keys = HashSet::new();
    let unique = input
        .blocks
        .iter()
        .all(|block| keys.insert(block_key(block)));
    if input.run_id != target.run_id
        || input.source_revision != target.source_revision
        || input.extractor_version != target.extractor_version
        || accounting.accounting_version != target.accounting_version
        || accounting.text_bytes != input.text_bytes
        || input.text_bytes != text_bytes
        || accounting.block_count != input.blocks.len() as u64
        || !unique
    {
        return Err(StorageError::Serialization(
            "thread index commit input does not match its guard".into(),
        ));
    }
    Ok(())
}

/// Diff replacement: upsert every current key (positions are rewritten even when
/// the hash matches), then delete keys that disappeared. FTS follows triggers.
fn replace_blocks(
    conn: &Connection,
    run_id: &str,
    blocks: &[IndexBlock],
) -> Result<(), StorageError> {
    let mut upsert = conn.prepare_cached(
        "INSERT INTO conversation_blocks(run_id, source_kind, source_key, ordinal, block_index, \
         note_seq, role, kind, tool_name, truncated, hash, text) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12) \
         ON CONFLICT(run_id, source_kind, source_key) DO UPDATE SET \
         ordinal = excluded.ordinal, block_index = excluded.block_index, \
         note_seq = excluded.note_seq, role = excluded.role, kind = excluded.kind, \
         tool_name = excluded.tool_name, truncated = excluded.truncated, \
         hash = excluded.hash, text = excluded.text",
    )?;
    let mut keep = HashSet::new();
    for block in blocks {
        let (source_kind, source_key) = block_key(block);
        let (ordinal, block_index, note_seq) = match block.locator {
            BlockLocator::Message {
                ordinal,
                block_index,
            } => (Some(ordinal), Some(block_index), None),
            BlockLocator::WorkingNote { note_seq } => (None, None, Some(note_seq)),
        };
        upsert.execute(params![
            run_id,
            source_kind,
            source_key,
            ordinal,
            block_index,
            note_seq,
            role_name(block.role),
            kind_name(block.kind),
            block.tool_name,
            block.truncated,
            block.hash,
            block.text,
        ])?;
        keep.insert((source_kind.to_owned(), source_key));
    }
    let existing = conn
        .prepare("SELECT id, source_kind, source_key FROM conversation_blocks WHERE run_id = ?1")?
        .query_map([run_id], |row| {
            Ok((row.get::<_, i64>(0)?, (row.get(1)?, row.get(2)?)))
        })?
        .collect::<Result<Vec<(i64, (String, String))>, _>>()?;
    let mut delete = conn.prepare_cached("DELETE FROM conversation_blocks WHERE id = ?1")?;
    for (id, key) in existing {
        if !keep.contains(&key) {
            delete.execute([id])?;
        }
    }
    Ok(())
}

impl ThreadIndexRepository for SqliteThreadIndex<'_> {
    fn discover_source(&mut self, header: &IndexSourceHeader) -> Result<(), StorageError> {
        let conn = self.conn;
        let run_id = header.target.run_id.as_str();
        atomic(conn, || match run_row(conn, run_id)? {
            None if header.source_present => insert_pending(conn, header),
            None => Ok(()),
            Some(row) if row.state == IndexState::Deleted => {
                if header.source_present {
                    conn.execute(
                        "UPDATE thread_index_runs SET source_after_tombstone = 1 WHERE run_id = ?1",
                        [run_id],
                    )?;
                }
                Ok(())
            }
            // Same revision (or older header): R2 "維持", including attempts.
            Some(row) if header.target.source_revision <= row.source_revision => Ok(()),
            Some(row) => {
                let to_pending = matches!(
                    index_transition(row.state, IndexEvent::SourceUpdated),
                    IndexTransition::Pending { .. }
                );
                update_source(
                    conn,
                    run_id,
                    &SourceValues {
                        source_revision: Some(header.target.source_revision),
                        snapshot_revision: Some(header.snapshot_revision),
                        sha256: Some(&header.context_sha256),
                        provenance_version: header.provenance_version,
                        note_seq: header.note_seq,
                        updated_at_ns: header.source_updated_at_ns,
                    },
                    to_pending,
                )
                .map(drop)
            }
        })
    }

    fn index_run_status(&self, run_id: &str) -> Result<Option<IndexRunStatus>, StorageError> {
        index_run_status(self.conn, run_id)
    }

    fn commit_index_input(
        &mut self,
        guard: &IndexCommitGuard,
        input: &ExtractedIndexInput,
        accounting: IndexAccounting,
        now_ns: i64,
    ) -> Result<IndexCommitResult, StorageError> {
        let conn = self.conn;
        atomic(conn, || {
            if let Some(rejection) = self.check_guard(guard, now_ns)? {
                return Ok(IndexCommitResult::Discarded(rejection));
            }
            validate_commit_input(guard, input, &accounting)?;
            let target = &guard.target;
            replace_blocks(conn, &target.run_id, &input.blocks)?;
            conn.execute(
                "UPDATE thread_index_runs SET state = 'indexed', indexed_revision = ?2, \
                 indexed_extractor_version = ?3, indexed_accounting_version = ?4, \
                 indexed_at_ns = ?5, text_bytes = ?6, attribute_bytes = ?7, block_count = ?8, \
                 charged_bytes = ?9, attempt_count = 0, next_attempt_at_ns = NULL, \
                 blocked_reason = NULL, last_error_code = NULL WHERE run_id = ?1",
                params![
                    target.run_id,
                    target.source_revision,
                    target.extractor_version,
                    accounting.accounting_version,
                    now_ns,
                    accounting.text_bytes,
                    accounting.attribute_bytes,
                    accounting.block_count,
                    accounting.charged_bytes,
                ],
            )?;
            self.close_request_target(guard, None)?;
            Ok(IndexCommitResult::Committed)
        })
    }

    fn record_index_failure(
        &mut self,
        guard: &IndexCommitGuard,
        failure: IndexFailure,
        now_ns: i64,
    ) -> Result<IndexCommitResult, StorageError> {
        let conn = self.conn;
        atomic(conn, || {
            if let Some(rejection) = self.check_guard(guard, now_ns)? {
                return Ok(IndexCommitResult::Discarded(rejection));
            }
            let attempts = run_row(conn, &guard.target.run_id)?.map_or(0, |row| row.attempt_count);
            match failure {
                IndexFailure::Transient => {
                    let attempts = attempts.saturating_add(1);
                    if attempts >= MAX_ATTEMPTS {
                        self.block(guard, IndexBlockedReason::AttemptsExhausted, attempts)?;
                    } else {
                        conn.execute(
                            "UPDATE thread_index_runs SET state = 'retry_wait', \
                             attempt_count = ?2, next_attempt_at_ns = ?3, \
                             last_error_code = 'transient_failure' WHERE run_id = ?1",
                            params![
                                guard.target.run_id,
                                attempts,
                                now_ns.saturating_add(retry_delay_ns(attempts))
                            ],
                        )?;
                    }
                }
                IndexFailure::Blocked(reason) => self.block(guard, reason, attempts)?,
            }
            Ok(IndexCommitResult::Committed)
        })
    }

    fn evict_index(
        &mut self,
        run_id: &str,
        reason: IndexEvictionReason,
    ) -> Result<(), StorageError> {
        let conn = self.conn;
        atomic(conn, || {
            let Some(row) = run_row(conn, run_id)? else {
                return Ok(());
            };
            if index_transition(row.state, IndexEvent::Evict) != IndexTransition::Evict {
                return Ok(());
            }
            self.drop_index(
                run_id,
                &format!(
                    "state = 'evicted', evicted_reason = '{}'",
                    eviction_name(reason)
                ),
            )
        })
    }

    fn delete_index_source(&mut self, run_id: &str) -> Result<(), StorageError> {
        let conn = self.conn;
        atomic(conn, || {
            match run_row(conn, run_id)? {
                Some(row) if row.state == IndexState::Deleted => return Ok(()),
                Some(_) => self.drop_index(
                    run_id,
                    "state = 'deleted', source_present = 0, evicted_reason = NULL",
                )?,
                None => {
                    let (extractor_version, accounting_version) = applied_versions(conn)?;
                    conn.execute(
                        "INSERT INTO thread_index_runs(run_id, state, source_revision, \
                         snapshot_revision, source_context_sha256, extractor_version, \
                         accounting_version, source_updated_at_ns, retention_at_ns, \
                         source_present, work_generation) \
                         VALUES (?1, 'deleted', 0, 0, '', ?2, ?3, 0, 0, 0, 1)",
                        params![run_id, extractor_version, accounting_version],
                    )?;
                }
            }
            conn.execute(
                "UPDATE thread_index_request_targets SET state = 'gap', gap_reason = ?2 \
                 WHERE run_id = ?1 AND state = 'pending'",
                params![run_id, to_json(&RequestGapReason::Deleted)?],
            )?;
            Ok(())
        })
    }
}

/// Removes up to `limit_rows` blocks (and at least one) of `run_id`'s index, stopping
/// once `limit_text_bytes` of text are taken. The FTS rows follow through the
/// blocks' delete trigger. The step only runs when the run's state allows
/// `target`; otherwise it does nothing. When the last block is gone the row
/// reaches its end state in the same transaction: `deleted` for a tombstoned
/// run, charges cleared (state kept) for an archived eviction.
///
/// It does not depend on the index job being enabled or leased: the caller is
/// the writer thread, which owns every write.
pub(crate) fn purge_index_rows(
    conn: &Connection,
    run_id: &str,
    target: PurgeTarget,
    limit_rows: usize,
    limit_text_bytes: u64,
) -> Result<PurgeProgress, StorageError> {
    purge_index_rows_with(
        conn,
        run_id,
        target,
        limit_rows,
        limit_text_bytes,
        OversizedBlock::Report,
    )
}

/// What a purge step does with a first block larger than its byte limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OversizedBlock {
    /// Leave it and report its size, so the caller can route it elsewhere.
    Report,
    /// Delete it alone: the exclusive phase, where one large row is the unit.
    TakeAlone,
}

/// [`purge_index_rows`] with a choice for a block above the byte limit. Either
/// way a step reads at most `limit_rows` blocks and deletes at most those that
/// fit the limit (or the one oversized block alone).
pub(crate) fn purge_index_rows_with(
    conn: &Connection,
    run_id: &str,
    target: PurgeTarget,
    limit_rows: usize,
    limit_text_bytes: u64,
    oversized_block: OversizedBlock,
) -> Result<PurgeProgress, StorageError> {
    let idle = PurgeProgress {
        rows: 0,
        text_bytes: 0,
        remaining: false,
        oversized: false,
        oversized_bytes: 0,
    };
    atomic(conn, || {
        let allowed: bool = match target {
            PurgeTarget::Tombstoned => conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id = ?1) \
                 AND EXISTS(SELECT 1 FROM thread_index_runs WHERE run_id = ?1)",
                [run_id],
                |row| row.get(0),
            )?,
            PurgeTarget::Archived => conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM thread_index_runs WHERE run_id = ?1 \
                 AND state = 'evicted' AND evicted_reason = 'archived')",
                [run_id],
                |row| row.get(0),
            )?,
        };
        if !allowed {
            return Ok(idle);
        }
        let limit = i64::try_from(limit_rows.max(1)).unwrap_or(i64::MAX);
        let mut ids = Vec::new();
        let mut text_bytes = 0_u64;
        let mut oversized = false;
        let mut oversized_bytes = 0_u64;
        {
            let mut statement = conn.prepare_cached(
                // The order is the unique key's, so the statement stops after `limit`
                // rows; ordering by id would sort every block of the run first.
                "SELECT id, length(CAST(text AS BLOB)) FROM conversation_blocks \
                 WHERE run_id = ?1 ORDER BY source_kind, source_key LIMIT ?2",
            )?;
            let mut rows = statement.query(params![run_id, limit])?;
            while let Some(row) = rows.next()? {
                let bytes = row.get::<_, u64>(1)?;
                if ids.is_empty() && bytes > limit_text_bytes {
                    if oversized_block == OversizedBlock::TakeAlone {
                        ids.push(row.get::<_, i64>(0)?);
                        text_bytes = bytes;
                        break;
                    }
                    // Never smuggle a block past the step's byte budget.
                    oversized = true;
                    oversized_bytes = bytes;
                    break;
                }
                if !ids.is_empty() && text_bytes.saturating_add(bytes) > limit_text_bytes {
                    break;
                }
                ids.push(row.get::<_, i64>(0)?);
                text_bytes = text_bytes.saturating_add(bytes);
            }
        }
        if !ids.is_empty() {
            let listed = serde_json::to_string(&ids)
                .map_err(|error| StorageError::Serialization(error.to_string()))?;
            conn.execute(
                "DELETE FROM conversation_blocks WHERE id IN (SELECT value FROM json_each(?1))",
                [listed],
            )?;
            // A tombstoned run's row may only be terminalised (see the fence on
            // `thread_index_runs`), so its charge stays until the last block is
            // gone; an archived eviction is free to shrink step by step.
            if target == PurgeTarget::Archived {
                conn.execute(
                    "UPDATE thread_index_runs SET text_bytes = max(0, text_bytes - ?2), \
                     charged_bytes = max(0, charged_bytes - ?2) WHERE run_id = ?1",
                    params![run_id, text_bytes],
                )?;
            }
        }
        let remaining: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM conversation_blocks WHERE run_id = ?1)",
            [run_id],
            |row| row.get(0),
        )?;
        let mut gapped = 0_usize;
        let mut finished = !remaining && !oversized;
        if finished {
            match target {
                PurgeTarget::Tombstoned => {
                    // Pending request targets of the run close as gaps, in batches
                    // that share the step's row budget with the blocks it deleted,
                    // so a run with many requests cannot make one step unbounded.
                    let room = limit_rows.max(1).saturating_sub(ids.len());
                    gapped = conn.execute(
                        "UPDATE thread_index_request_targets SET state = 'gap', gap_reason = ?2 \
                         WHERE run_id = ?1 AND state = 'pending' AND request_id IN (\
                             SELECT request_id FROM thread_index_request_targets \
                             WHERE run_id = ?1 AND state = 'pending' LIMIT ?3)",
                        params![
                            run_id,
                            to_json(&RequestGapReason::Deleted)?,
                            i64::try_from(room).unwrap_or(i64::MAX)
                        ],
                    )?;
                    let pending: bool = conn.query_row(
                        "SELECT EXISTS(SELECT 1 FROM thread_index_request_targets \
                         WHERE run_id = ?1 AND state = 'pending')",
                        [run_id],
                        |row| row.get(0),
                    )?;
                    if pending {
                        finished = false;
                    } else {
                        SqliteThreadIndex::new(conn).drop_index(
                            run_id,
                            "state = 'deleted', source_present = 0, evicted_reason = NULL",
                        )?;
                    }
                }
                PurgeTarget::Archived => {
                    conn.execute(
                        "UPDATE thread_index_runs SET text_bytes = 0, attribute_bytes = 0, \
                         block_count = 0, charged_bytes = 0, indexed_revision = NULL, \
                         indexed_extractor_version = NULL, indexed_accounting_version = NULL, \
                         indexed_at_ns = NULL, work_generation = work_generation + 1 \
                         WHERE run_id = ?1",
                        [run_id],
                    )?;
                }
            }
        }
        Ok(PurgeProgress {
            rows: ids.len() + gapped,
            text_bytes,
            remaining: !finished,
            oversized,
            oversized_bytes,
        })
    })
}
