//! SQL of the lane ledger (see [`crate::lanes`] for the contract).

use rusqlite::{Connection, OptionalExtension, Row, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use crate::StorageError;
use crate::entity::SecretGuard;
use crate::error::{LaneConflictReason, LaneCursorError};
use crate::lanes::{
    LANE_ID_MAX_BYTES, LANE_LIST_SNAPSHOTS_MAX, LANE_LIST_SNAPSHOTS_PER_LEAD,
    LANE_OBJECTIVE_PREVIEW_MAX_BYTES, LANE_REPORTS_PAGE_MAX, LANE_START_SEED_MAX_BYTES, LaneBrief,
    LaneCreation, LaneListItem, LaneListPage, LaneListRequest, LanePolicy, LaneRecord, LaneReport,
    LaneReportAppended, LaneReportInput, LaneReportProducer, LaneScope, LaneStatus, LaneUpdate,
    NewLane,
};
use event_bus::{
    LANE_LIST_PAGE_MAX, LANE_LIST_PAGE_MAX_BYTES, LaneValidationError, LaneValidationKind,
};

/// A lane is visible while its thread is not tombstoned.
const LIVE: &str = "thread_id NOT IN (SELECT thread_id FROM thread_tombstones)";

const COLUMNS: &str = "lane_id, thread_id, origin_thread_id, origin_run_id, creation_key, scope, \
    lead_thread_id, policy, management_revision, brief_json, brief_revision, start_seed, \
    root_run_id, generation, latest_report_seq, created_at_ns, updated_at_ns, last_activity_at_ns, start_blocked_reason, detached_since_management_revision";

const REPORT_COLUMNS: &str = "lane_id, report_seq, status, summary, artifact_refs_json, \
    needs_human, question_refs_json, reason_code, producer, management_revision, brief_revision, \
    created_at_ns, artifact_revision_json, failure_episode";

fn immediate(conn: &Connection) -> Result<Transaction<'_>, StorageError> {
    Ok(Transaction::new_unchecked(
        conn,
        TransactionBehavior::Immediate,
    )?)
}

fn invalid(field: &'static str, kind: LaneValidationKind) -> StorageError {
    StorageError::LaneInvalid(LaneValidationError::new(field, kind))
}

fn check_id(field: &'static str, value: &str) -> Result<(), StorageError> {
    if value.trim().is_empty() {
        return Err(invalid(field, LaneValidationKind::Empty));
    }
    if value.len() > LANE_ID_MAX_BYTES {
        return Err(invalid(
            field,
            LaneValidationKind::TooLarge {
                max: LANE_ID_MAX_BYTES,
                actual: value.len(),
            },
        ));
    }
    Ok(())
}

fn guard_texts(
    entity: &'static str,
    texts: impl IntoIterator<Item = (&'static str, impl AsRef<str>)>,
) -> Result<(), StorageError> {
    let guard = SecretGuard::from_env();
    texts
        .into_iter()
        .try_for_each(|(field, text)| guard.check_text(entity, field, text.as_ref()))
}

fn to_i64(value: u64, what: &'static str) -> Result<i64, StorageError> {
    i64::try_from(value).map_err(|_| StorageError::OutOfRange(what))
}

fn hash_hex(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.len().to_le_bytes());
        hasher.update(part.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

fn bump_revision(tx: &Transaction<'_>) -> Result<u64, StorageError> {
    let revision: i64 = tx.query_row(
        "UPDATE lane_state SET revision = revision + 1 WHERE id = 1 RETURNING revision",
        [],
        |row| row.get(0),
    )?;
    u64::try_from(revision).map_err(|_| StorageError::OutOfRange("lane ledger revision"))
}

fn ledger_revision(conn: &Connection) -> Result<u64, StorageError> {
    let revision: i64 =
        conn.query_row("SELECT revision FROM lane_state WHERE id = 1", [], |row| {
            row.get(0)
        })?;
    u64::try_from(revision).map_err(|_| StorageError::OutOfRange("lane ledger revision"))
}

fn column_error(index: usize, name: &'static str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        index,
        rusqlite::types::Type::Text,
        format!("unknown lane {name}").into(),
    )
}

fn count(row: &Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(index)?;
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(index, value))
}

fn from_row(row: &Row<'_>) -> rusqlite::Result<LaneRecord> {
    record_at(row, 0)
}

/// Decodes the [`COLUMNS`] of a lane that start at column `offset` of the row.
fn record_at(row: &Row<'_>, offset: usize) -> rusqlite::Result<LaneRecord> {
    let at = |index: usize| index + offset;
    let scope: String = row.get(at(5))?;
    let policy_label: String = row.get(at(7))?;
    let policy = LanePolicy::parse(&policy_label).ok_or_else(|| column_error(at(7), "policy"))?;
    let brief_json: String = row.get(at(9))?;
    let mut brief: LaneBrief = serde_json::from_str(&brief_json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            at(9),
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })?;
    // The brief shows the policy the lane has now, not the one it was created with.
    brief.report_policy = policy;
    Ok(LaneRecord {
        lane_id: row.get(at(0))?,
        thread_id: row.get(at(1))?,
        origin_thread_id: row.get(at(2))?,
        origin_run_id: row.get(at(3))?,
        creation_key: row.get(at(4))?,
        scope: LaneScope::parse(&scope).ok_or_else(|| column_error(at(5), "scope"))?,
        lead_thread_id: row.get(at(6))?,
        policy,
        management_revision: count(row, at(8))?,
        brief,
        brief_revision: count(row, at(10))?,
        start_seed: row.get(at(11))?,
        root_run_id: row.get(at(12))?,
        generation: count(row, at(13))?,
        latest_report_seq: count(row, at(14))?,
        created_at_ns: row.get(at(15))?,
        updated_at_ns: row.get(at(16))?,
        last_activity_at_ns: row.get(at(17))?,
        start_blocked_reason: row.get(at(18))?,
        detached_since_management_revision: row.get(at(19))?,
    })
}

fn report_from_row(row: &Row<'_>) -> rusqlite::Result<LaneReport> {
    let json_list = |index: usize| -> rusqlite::Result<Vec<String>> {
        let text: String = row.get(index)?;
        serde_json::from_str(&text).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                index,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })
    };
    let status: String = row.get(2)?;
    let producer: String = row.get(8)?;
    Ok(LaneReport {
        artifact_revision: row
            .get::<_, Option<String>>(12)?
            .map(|s| {
                serde_json::from_str(&s).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        12,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })
            })
            .transpose()?,
        failure_episode: row.get(13)?,
        lane_id: row.get(0)?,
        report_seq: count(row, 1)?,
        status: LaneStatus::parse(&status).ok_or_else(|| column_error(2, "status"))?,
        summary: row.get(3)?,
        artifact_refs: json_list(4)?,
        needs_human: row.get::<_, i64>(5)? != 0,
        question_refs: json_list(6)?,
        reason_code: row.get(7)?,
        producer: LaneReportProducer::parse(&producer)
            .ok_or_else(|| column_error(8, "producer"))?,
        management_revision: count(row, 9)?,
        brief_revision: count(row, 10)?,
        created_at_ns: row.get(11)?,
    })
}

pub(crate) fn get(conn: &Connection, lane_id: &str) -> Result<Option<LaneRecord>, StorageError> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM lanes WHERE lane_id = ?1 AND {LIVE}"),
            [lane_id],
            from_row,
        )
        .optional()?)
}

/// Internal live membership scan; unlike lane_list it creates no cursor snapshot.
pub(crate) fn managed_lanes(
    conn: &Connection,
    lead: &str,
) -> Result<Vec<LaneRecord>, StorageError> {
    let mut statement = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM lanes WHERE lead_thread_id = ?1 AND {LIVE} ORDER BY lane_id"
    ))?;
    statement
        .query_map([lead], from_row)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}

/// Identity lookup deliberately ignores current lead/policy: detached intents
/// still own their original creation key.
pub(crate) fn get_by_creation_key(
    conn: &Connection,
    origin_thread_id: &str,
    creation_key: &str,
) -> Result<Option<LaneRecord>, StorageError> {
    let id: Option<String> = conn
        .query_row(
            "SELECT lane_id FROM lanes WHERE origin_thread_id = ?1 AND creation_key = ?2",
            params![origin_thread_id, creation_key],
            |row| row.get(0),
        )
        .optional()?;
    id.map(|id| get(conn, &id)).transpose().map(Option::flatten)
}

pub(crate) fn get_by_thread(
    conn: &Connection,
    thread_id: &str,
) -> Result<Option<LaneRecord>, StorageError> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM lanes WHERE thread_id = ?1 AND {LIVE}"),
            [thread_id],
            from_row,
        )
        .optional()?)
}

fn require(conn: &Connection, lane_id: &str) -> Result<LaneRecord, StorageError> {
    get(conn, lane_id)?.ok_or_else(|| StorageError::LaneNotFound {
        lane_id: lane_id.to_owned(),
    })
}

/// Lanes whose root never started, in lane ID order: creation intents a crash left.
pub(crate) fn unstarted(conn: &Connection, limit: usize) -> Result<Vec<LaneRecord>, StorageError> {
    let limit = i64::try_from(limit.clamp(1, 200)).unwrap_or(200);
    let mut statement = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM lanes WHERE generation = 0 AND start_blocked_reason IS NULL AND NOT EXISTS (SELECT 1 FROM lane_handoffs h WHERE h.lane_id=lanes.lane_id AND h.state IN ('preparing','prepared')) AND {LIVE} ORDER BY lane_id LIMIT ?1"
    ))?;
    Ok(statement
        .query_map([limit], from_row)?
        .collect::<Result<_, _>>()?)
}

pub(crate) fn reports(
    conn: &Connection,
    lane_id: &str,
    after_seq: u64,
    limit: usize,
) -> Result<Vec<LaneReport>, StorageError> {
    let limit = i64::try_from(limit.clamp(1, LANE_REPORTS_PAGE_MAX)).unwrap_or(1);
    let after = to_i64(after_seq, "report sequence")?;
    let mut statement = conn.prepare(&format!(
        "SELECT {REPORT_COLUMNS} FROM lane_reports WHERE lane_id = ?1 AND report_seq > ?2 \
         AND lane_id IN (SELECT lane_id FROM lanes WHERE {LIVE}) \
         ORDER BY report_seq LIMIT ?3"
    ))?;
    Ok(statement
        .query_map(params![lane_id, after, limit], report_from_row)?
        .collect::<Result<_, _>>()?)
}

fn latest_report(conn: &Connection, lane: &LaneRecord) -> Result<Option<LaneReport>, StorageError> {
    if lane.latest_report_seq == 0 {
        return Ok(None);
    }
    Ok(conn
        .query_row(
            &format!(
                "SELECT {REPORT_COLUMNS} FROM lane_reports WHERE lane_id = ?1 AND report_seq = ?2"
            ),
            params![
                lane.lane_id,
                to_i64(lane.latest_report_seq, "report sequence")?
            ],
            report_from_row,
        )
        .optional()?)
}

fn tombstone_threads(conn: &Connection, thread_ids: &[&str]) -> Result<(), StorageError> {
    for thread_id in thread_ids {
        let hit: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM thread_tombstones WHERE thread_id = ?1)",
            [thread_id],
            |row| row.get(0),
        )?;
        if hit {
            return Err(StorageError::Tombstoned {
                run_id: None,
                thread_id: Some((*thread_id).to_owned()),
            });
        }
    }
    Ok(())
}

fn tombstone_check(
    conn: &Connection,
    thread_ids: &[&str],
    run_id: &str,
) -> Result<(), StorageError> {
    tombstone_threads(conn, thread_ids)?;
    let hit: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id = ?1)",
        [run_id],
        |row| row.get(0),
    )?;
    if hit {
        return Err(StorageError::Tombstoned {
            run_id: Some(run_id.to_owned()),
            thread_id: None,
        });
    }
    Ok(())
}

pub(crate) fn create(conn: &Connection, new: &NewLane) -> Result<LaneCreation, StorageError> {
    for (field, value) in [
        ("lane_id", &new.lane_id),
        ("thread_id", &new.thread_id),
        ("origin_thread_id", &new.origin_thread_id),
        ("origin_run_id", &new.origin_run_id),
        ("creation_key", &new.creation_key),
    ] {
        check_id(field, value)?;
    }
    new.brief.validate().map_err(StorageError::LaneInvalid)?;
    if new.start_seed.len() > LANE_START_SEED_MAX_BYTES {
        return Err(invalid(
            "start_seed",
            LaneValidationKind::TooLarge {
                max: LANE_START_SEED_MAX_BYTES,
                actual: new.start_seed.len(),
            },
        ));
    }
    guard_texts("lane_brief", new.brief.guarded_texts())?;
    guard_texts("lane", [("start_seed", new.start_seed.as_str())])?;
    guard_texts("lane", [("creation_key", new.creation_key.as_str())])?;
    let title = new
        .title
        .as_deref()
        .map(|title| secret_guard::SecretRedactor::from_env().redact(title).text);
    let policy = new.brief.report_policy;
    let lead = policy.is_managed().then(|| new.origin_thread_id.clone());
    let brief_json = serde_json::to_string(&new.brief)
        .map_err(|error| StorageError::Serialization(error.to_string()))?;
    let request_hash = hash_hex(&[&brief_json]);

    let tx = immediate(conn)?;
    let existing: Option<(String, String)> = tx
        .query_row(
            "SELECT lane_id, request_hash FROM lanes WHERE origin_thread_id = ?1 AND creation_key = ?2",
            params![new.origin_thread_id, new.creation_key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((lane_id, stored_hash)) = existing {
        if stored_hash != request_hash {
            return Err(StorageError::LaneConflict(
                LaneConflictReason::CreationKeyReused,
            ));
        }
        let lane = require(&tx, &lane_id)?;
        return Ok(LaneCreation {
            lane,
            created: false,
        });
    }
    tombstone_check(
        &tx,
        &[&new.thread_id, &new.origin_thread_id],
        &new.origin_run_id,
    )?;
    let taken: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM lanes WHERE lane_id = ?1 OR thread_id = ?2)",
        params![new.lane_id, new.thread_id],
        |row| row.get(0),
    )?;
    if taken {
        return Err(StorageError::LaneConflict(
            LaneConflictReason::IdentityTaken,
        ));
    }
    let written = tx.execute(
        "INSERT INTO threads (thread_id, project_id, project_root, title, chat_role, lineage_kind, \
            created_at_ns, updated_at_ns, source) \
         VALUES (?1, ?2, ?3, ?4, 'Orchestrator', 'lane', ?5, ?5, 'runtime') \
         ON CONFLICT(thread_id) DO UPDATE SET lineage_kind = 'lane', \
            project_id = COALESCE(threads.project_id, excluded.project_id), \
            project_root = COALESCE(threads.project_root, excluded.project_root), \
            title = COALESCE(threads.title, excluded.title), \
            chat_role = COALESCE(threads.chat_role, excluded.chat_role) \
         WHERE threads.lineage_kind IS NULL",
        params![
            new.thread_id,
            new.project_id,
            new.project_root,
            title,
            new.at_ns
        ],
    )?;
    if written == 0 {
        return Err(StorageError::LaneConflict(
            LaneConflictReason::ThreadNotLane,
        ));
    }
    tx.execute(
        "INSERT INTO lanes (lane_id, thread_id, origin_thread_id, origin_run_id, creation_key, \
            request_hash, scope, lead_thread_id, policy, management_revision, brief_json, \
            brief_revision, start_seed, created_at_ns, updated_at_ns, last_activity_at_ns) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'lane', ?7, ?8, 1, ?9, 1, ?10, ?11, ?11, ?11)",
        params![
            new.lane_id,
            new.thread_id,
            new.origin_thread_id,
            new.origin_run_id,
            new.creation_key,
            request_hash,
            lead,
            policy.as_str(),
            brief_json,
            new.start_seed,
            new.at_ns,
        ],
    )?;
    bump_revision(&tx)?;
    let lane = require(&tx, &new.lane_id)?;
    tx.commit()?;
    Ok(LaneCreation {
        lane,
        created: true,
    })
}

/// Records that the root run started (or restarted): the generation moves from
/// `expected_generation` to the next one and the root run becomes `root_run_id`.
pub(crate) fn start_run(
    conn: &Connection,
    lane_id: &str,
    expected_generation: u64,
    root_run_id: &str,
    at_ns: i64,
) -> Result<LaneRecord, StorageError> {
    check_id("root_run_id", root_run_id)?;
    let tx = immediate(conn)?;
    let lane = require(&tx, lane_id)?;
    if lane.start_blocked_reason.is_some() {
        return Err(StorageError::LaneConflict(LaneConflictReason::StartBlocked));
    }
    if lane.generation != expected_generation {
        return Err(StorageError::StaleRevision {
            kind: "generation",
            expected: expected_generation,
            current: lane.generation,
        });
    }
    tx.execute(
        "UPDATE lanes SET generation = generation + 1, root_run_id = ?2, \
            updated_at_ns = max(updated_at_ns, ?3), \
            last_activity_at_ns = max(last_activity_at_ns, ?3) WHERE lane_id = ?1",
        params![lane_id, root_run_id, at_ns],
    )?;
    tx.execute("UPDATE lane_handoffs SET state='started',failure=NULL WHERE lane_id=?1 AND state IN ('committed','recoverable_start_failure')",[lane_id])?;
    bump_revision(&tx)?;
    let lane = require(&tx, lane_id)?;
    tx.commit()?;
    Ok(lane)
}

/// Permanently exclude a never-started intent from automatic recovery.
pub(crate) fn block_start(
    conn: &Connection,
    lane_id: &str,
    reason: &str,
    at_ns: i64,
) -> Result<LaneUpdate, StorageError> {
    if reason.trim().is_empty() {
        return Err(invalid("start_blocked_reason", LaneValidationKind::Empty));
    }
    if reason.len() > crate::lanes::LANE_SUMMARY_MAX_BYTES {
        return Err(invalid(
            "start_blocked_reason",
            LaneValidationKind::TooLarge {
                max: crate::lanes::LANE_SUMMARY_MAX_BYTES,
                actual: reason.len(),
            },
        ));
    }
    guard_texts("lane startup", [("start_blocked_reason", reason)])?;
    let tx = immediate(conn)?;
    let lane = require(&tx, lane_id)?;
    // The first reason wins, except that an operator stop (resumable) hardens into a
    // cancellation or a failure (permanent); the reverse never loosens anything.
    let hardens = lane.is_operator_stopped() && reason != crate::lanes::LANE_START_OPERATOR_STOP;
    if lane.is_started() || (lane.start_blocked_reason.is_some() && !hardens) {
        return Ok(LaneUpdate {
            lane,
            changed: false,
        });
    }
    tx.execute(
        "UPDATE lanes SET start_blocked_reason = ?2, updated_at_ns = max(updated_at_ns, ?3) WHERE lane_id = ?1",
        params![lane_id, reason, at_ns],
    )?;
    bump_revision(&tx)?;
    let lane = require(&tx, lane_id)?;
    tx.commit()?;
    Ok(LaneUpdate {
        lane,
        changed: true,
    })
}

/// Lets an operator-stopped, never-started intent be recovered and started again.
/// A started lane or an unblocked intent is unchanged; an intent blocked for another
/// reason (cancelled, locally failed) stays blocked.
pub(crate) fn resume_start(
    conn: &Connection,
    lane_id: &str,
    at_ns: i64,
) -> Result<LaneUpdate, StorageError> {
    let tx = immediate(conn)?;
    let lane = require(&tx, lane_id)?;
    if lane.is_started() || lane.start_blocked_reason.is_none() {
        return Ok(LaneUpdate {
            lane,
            changed: false,
        });
    }
    if !lane.is_operator_stopped() {
        return Err(StorageError::LaneConflict(LaneConflictReason::StartBlocked));
    }
    tx.execute(
        "UPDATE lanes SET start_blocked_reason = NULL, updated_at_ns = max(updated_at_ns, ?2) WHERE lane_id = ?1",
        params![lane_id, at_ns],
    )?;
    bump_revision(&tx)?;
    let lane = require(&tx, lane_id)?;
    tx.commit()?;
    Ok(LaneUpdate {
        lane,
        changed: true,
    })
}

pub(crate) fn append_report(
    conn: &Connection,
    lane_id: &str,
    expected_management_revision: u64,
    input: &LaneReportInput,
    at_ns: i64,
) -> Result<LaneReportAppended, StorageError> {
    let tx = immediate(conn)?;
    let result =
        append_report_in_transaction(&tx, lane_id, expected_management_revision, input, at_ns)?;
    tx.commit()?;
    Ok(result)
}

/// Caller owns the transaction; used to commit report + semantic key + outbox.
pub(crate) fn append_report_in_transaction(
    tx: &Transaction<'_>,
    lane_id: &str,
    expected_management_revision: u64,
    input: &LaneReportInput,
    at_ns: i64,
) -> Result<LaneReportAppended, StorageError> {
    input.validate().map_err(StorageError::LaneInvalid)?;
    guard_texts("lane_report", input.guarded_texts())?;
    let artifact_refs = serde_json::to_string(&input.artifact_refs)
        .map_err(|error| StorageError::Serialization(error.to_string()))?;
    let question_refs = serde_json::to_string(&input.question_refs)
        .map_err(|error| StorageError::Serialization(error.to_string()))?;
    let lane = require(tx, lane_id)?;
    if lane.management_revision != expected_management_revision {
        return Err(StorageError::LaneManagement(
            crate::lane_management::LaneManagementError::StaleManagementRevision,
        ));
    }
    if !lane.policy.is_managed() {
        return Err(StorageError::LaneManagement(
            crate::lane_management::LaneManagementError::InvalidPolicyTransition,
        ));
    }
    let seq = lane.latest_report_seq + 1;
    let seq_param = to_i64(seq, "report sequence")?;
    // The limit is on the report as stored and returned, identity and counters included.
    LaneReport {
        artifact_revision: input.artifact_revision.clone(),
        failure_episode: input.failure_episode.clone(),
        lane_id: lane_id.to_owned(),
        report_seq: seq,
        status: input.status,
        summary: input.summary.clone(),
        artifact_refs: input.artifact_refs.clone(),
        needs_human: input.needs_human,
        question_refs: input.question_refs.clone(),
        reason_code: input.reason_code.clone(),
        producer: input.producer,
        management_revision: lane.management_revision,
        brief_revision: lane.brief_revision,
        created_at_ns: at_ns,
    }
    .validate_size()
    .map_err(StorageError::LaneInvalid)?;
    tx.execute(
        &format!(
            "INSERT INTO lane_reports ({REPORT_COLUMNS}) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)"
        ),
        params![
            lane_id,
            seq_param,
            input.status.as_str(),
            input.summary,
            artifact_refs,
            input.needs_human,
            question_refs,
            input.reason_code,
            input.producer.as_str(),
            to_i64(lane.management_revision, "management revision")?,
            to_i64(lane.brief_revision, "brief revision")?,
            at_ns,
            input
                .artifact_revision
                .as_ref()
                .map(serde_json::to_string)
                .transpose()
                .map_err(|e| StorageError::Serialization(e.to_string()))?,
            input.failure_episode,
        ],
    )?;
    tx.execute(
        "UPDATE lanes SET latest_report_seq = ?2, updated_at_ns = max(updated_at_ns, ?3), \
            last_activity_at_ns = max(last_activity_at_ns, ?3) WHERE lane_id = ?1",
        params![lane_id, seq_param, at_ns],
    )?;
    crate::repo::lane_control::report_progress(tx, lane_id, lane.latest_report_seq, input, at_ns)?;
    bump_revision(tx)?;
    let report = tx.query_row(
        &format!(
            "SELECT {REPORT_COLUMNS} FROM lane_reports WHERE lane_id = ?1 AND report_seq = ?2"
        ),
        params![lane_id, seq_param],
        report_from_row,
    )?;
    let lane = require(tx, lane_id)?;
    Ok(LaneReportAppended { lane, report })
}

pub(crate) fn revise_brief(
    conn: &Connection,
    lane_id: &str,
    expected_revision: u64,
    brief: &LaneBrief,
    at_ns: i64,
) -> Result<LaneUpdate, StorageError> {
    let tx = immediate(conn)?;
    let update = revise_brief_in(&tx, lane_id, expected_revision, brief, at_ns)?;
    tx.commit()?;
    Ok(update)
}

/// The brief replacement inside the caller's transaction, which owns the commit. An
/// instruction acceptance needs the brief and its delivery reservation in one commit.
pub(crate) fn revise_brief_in(
    tx: &Transaction<'_>,
    lane_id: &str,
    expected_revision: u64,
    brief: &LaneBrief,
    at_ns: i64,
) -> Result<LaneUpdate, StorageError> {
    brief.validate().map_err(StorageError::LaneInvalid)?;
    guard_texts("lane_brief", brief.guarded_texts())?;
    let lane = require(tx, lane_id)?;
    if lane.brief_revision != expected_revision {
        return Err(StorageError::StaleRevision {
            kind: "brief",
            expected: expected_revision,
            current: lane.brief_revision,
        });
    }
    if brief.report_policy != lane.policy {
        return Err(invalid(
            "report_policy",
            LaneValidationKind::Invalid("must equal the lane's current policy"),
        ));
    }
    if *brief == lane.brief {
        return Ok(LaneUpdate {
            lane,
            changed: false,
        });
    }
    let brief_json = serde_json::to_string(brief)
        .map_err(|error| StorageError::Serialization(error.to_string()))?;
    tx.execute(
        "UPDATE lanes SET brief_json = ?2, brief_revision = brief_revision + 1, \
            updated_at_ns = max(updated_at_ns, ?3) WHERE lane_id = ?1",
        params![lane_id, brief_json, at_ns],
    )?;
    bump_revision(tx)?;
    let lane = require(tx, lane_id)?;
    Ok(LaneUpdate {
        lane,
        changed: true,
    })
}

/// Records real activity at `at_ns` (the stored time only moves forward).
pub(crate) fn touch_activity(
    conn: &Connection,
    lane_id: &str,
    at_ns: i64,
) -> Result<LaneRecord, StorageError> {
    let tx = immediate(conn)?;
    let lane = require(&tx, lane_id)?;
    if at_ns <= lane.last_activity_at_ns {
        return Ok(lane);
    }
    tx.execute(
        "UPDATE lanes SET last_activity_at_ns = ?2, updated_at_ns = max(updated_at_ns, ?2) \
         WHERE lane_id = ?1",
        params![lane_id, at_ns],
    )?;
    bump_revision(&tx)?;
    let lane = require(&tx, lane_id)?;
    tx.commit()?;
    Ok(lane)
}

/// Registers the semantic key of a notification for the management revision the caller
/// read; `true` when the key is new. A repeat changes nothing.
pub(crate) fn register_delivery_key(
    conn: &Connection,
    lane_id: &str,
    management_revision: u64,
    event_kind: &str,
    dedupe_key: &str,
    at_ns: i64,
) -> Result<bool, StorageError> {
    check_id("event_kind", event_kind)?;
    check_id("dedupe_key", dedupe_key)?;
    let tx = immediate(conn)?;
    let lane = require(&tx, lane_id)?;
    if lane.management_revision != management_revision {
        return Err(StorageError::StaleRevision {
            kind: "management",
            expected: management_revision,
            current: lane.management_revision,
        });
    }
    let revision = ledger_revision(&tx)?;
    let inserted = tx.execute(
        "INSERT OR IGNORE INTO lane_delivery_keys \
            (lane_id, management_revision, event_kind, dedupe_key, ledger_revision, created_at_ns) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            lane_id,
            to_i64(management_revision, "management revision")?,
            event_kind,
            dedupe_key,
            to_i64(revision, "lane ledger revision")?,
            at_ns,
        ],
    )?;
    tx.commit()?;
    Ok(inserted > 0)
}

/// The start of the objective whose serialized form is at most
/// [`LANE_OBJECTIVE_PREVIEW_MAX_BYTES`] bytes (escapes counted), cut at a character
/// boundary.
fn preview(objective: &str) -> String {
    let mut used = 0;
    let mut preview = String::new();
    for character in objective.chars() {
        let mut buffer = [0_u8; 4];
        let escaped = serde_json::to_string(character.encode_utf8(&mut buffer))
            .map_or(usize::MAX, |json| json.len() - 2);
        if used + escaped > LANE_OBJECTIVE_PREVIEW_MAX_BYTES {
            break;
        }
        used += escaped;
        preview.push(character);
    }
    preview
}

fn item(conn: &Connection, lane: LaneRecord) -> Result<LaneListItem, StorageError> {
    let latest_report = latest_report(conn, &lane)?;
    Ok(LaneListItem {
        objective_preview: preview(&lane.brief.objective),
        workspace: lane.brief.workspace,
        lane_id: lane.lane_id,
        thread_id: lane.thread_id,
        policy: lane.policy,
        management_revision: lane.management_revision,
        brief_revision: lane.brief_revision,
        root_run_id: lane.root_run_id,
        generation: lane.generation,
        created_at_ns: lane.created_at_ns,
        last_activity_at_ns: lane.last_activity_at_ns,
        latest_report,
    })
}

fn filter_hash(request: &LaneListRequest) -> String {
    let policy = request.policy.map_or("", LanePolicy::as_str);
    hash_hex(&["lane_list_v1", &request.lead_thread_id, policy])[..16].to_owned()
}

fn encode_cursor(snapshot_id: i64, position: i64, filter: &str) -> String {
    format!("l1.{snapshot_id}.{position}.{filter}")
}

fn decode_cursor(token: &str) -> Result<(i64, i64, &str), StorageError> {
    let malformed = || StorageError::LaneCursor(LaneCursorError::Malformed);
    let mut parts = token.split('.');
    let (Some("l1"), Some(snapshot), Some(position), Some(filter), None) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return Err(malformed());
    };
    let snapshot = snapshot.parse().map_err(|_| malformed())?;
    let position = position.parse().map_err(|_| malformed())?;
    if position < 0 {
        return Err(malformed());
    }
    Ok((snapshot, position, filter))
}

/// The longest cursor [`encode_cursor`] can produce, for sizing a page before its cursor
/// exists.
fn worst_cursor() -> String {
    encode_cursor(i64::MAX, i64::MAX, &"f".repeat(16))
}

/// Takes entries from `rows` (position, lane) while the entry count and the serialized
/// size of the whole page (items, the cursor it may carry and its fixed fields) stay
/// inside the limits. Returns the items and the position of the first entry left out.
///
/// # Errors
/// A single entry that cannot fit a page by itself is an error, never returned
/// anyway: write-side limits keep an entry far below the budget, so this means a
/// stored lane that breaks them.
fn fill_page(
    conn: &Connection,
    rows: Vec<(i64, LaneRecord)>,
    limit: usize,
    snapshot_revision: u64,
) -> Result<(Vec<LaneListItem>, Option<i64>), StorageError> {
    let worst = worst_cursor();
    let mut items: Vec<LaneListItem> = Vec::new();
    let mut next = None;
    for (position, lane) in rows {
        if items.len() >= limit {
            next = Some(position);
            break;
        }
        let entry = item(conn, lane)?;
        let mut candidate = items.clone();
        candidate.push(entry);
        let page = LaneListPage {
            items: candidate,
            next_cursor: Some(worst.clone()),
            has_more: true,
            snapshot_revision,
        };
        let size = serde_json::to_vec(&page)
            .map_err(|error| StorageError::Serialization(error.to_string()))?
            .len();
        if size > LANE_LIST_PAGE_MAX_BYTES {
            if items.is_empty() {
                return Err(invalid(
                    "lane_list_item",
                    LaneValidationKind::TooLarge {
                        max: LANE_LIST_PAGE_MAX_BYTES,
                        actual: size,
                    },
                ));
            }
            next = Some(position);
            break;
        }
        items = page.items;
    }
    Ok((items, next))
}

pub(crate) fn list(
    conn: &Connection,
    request: &LaneListRequest,
) -> Result<LaneListPage, StorageError> {
    check_id("lead_thread_id", &request.lead_thread_id)?;
    let limit = request.limit.clamp(1, LANE_LIST_PAGE_MAX);
    let fetch = i64::try_from(limit + 1).unwrap_or(i64::MAX);
    let filter = filter_hash(request);
    let policy = request.policy.map(LanePolicy::as_str);
    let tx = immediate(conn)?;
    let page = match &request.cursor {
        Some(token) => {
            let (snapshot_id, position, cursor_filter) = decode_cursor(token)?;
            if cursor_filter != filter {
                return Err(StorageError::LaneCursor(LaneCursorError::FilterMismatch));
            }
            let as_of: Option<i64> = tx
                .query_row(
                    "SELECT as_of_revision FROM lane_list_snapshots \
                     WHERE snapshot_id = ?1 AND filter_hash = ?2 AND lead_thread_id = ?3",
                    params![snapshot_id, filter, request.lead_thread_id],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(as_of) = as_of else {
                return Err(StorageError::LaneCursor(LaneCursorError::Stale));
            };
            // Membership/order are frozen. Management edits invalidate the snapshot
            // transactionally, so a later page cannot bypass the original policy filter.
            let mut statement = tx.prepare(&format!(
                "SELECT r.position, {} FROM lane_list_snapshot_rows r \
                 JOIN lanes l ON l.lane_id = r.lane_id \
                 WHERE r.snapshot_id = ?1 AND r.position >= ?2 AND l.lead_thread_id = ?3 \
                   AND l.thread_id NOT IN (SELECT thread_id FROM thread_tombstones) \
                 ORDER BY r.position LIMIT ?4",
                COLUMNS
                    .split(", ")
                    .map(|column| format!("l.{column}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))?;
            let rows = statement
                .query_map(
                    params![snapshot_id, position, request.lead_thread_id, fetch],
                    |row| {
                        let position: i64 = row.get(0)?;
                        let lane = record_at(row, 1)?;
                        Ok((position, lane))
                    },
                )?
                .collect::<Result<Vec<_>, _>>()?;
            drop(statement);
            let (items, next) = fill_page(
                &tx,
                rows,
                limit,
                u64::try_from(as_of)
                    .map_err(|_| StorageError::OutOfRange("lane ledger revision"))?,
            )?;
            LaneListPage {
                items,
                next_cursor: next.map(|next| encode_cursor(snapshot_id, next, &filter)),
                has_more: next.is_some(),
                snapshot_revision: u64::try_from(as_of)
                    .map_err(|_| StorageError::OutOfRange("lane ledger revision"))?,
            }
        }
        None => first_page(&tx, request, limit, fetch, &filter, policy)?,
    };
    tx.commit()?;
    Ok(page)
}

fn first_page(
    tx: &Transaction<'_>,
    request: &LaneListRequest,
    limit: usize,
    fetch: i64,
    filter: &str,
    policy: Option<&str>,
) -> Result<LaneListPage, StorageError> {
    let as_of = ledger_revision(tx)?;
    let mut statement = tx.prepare(&format!(
        "SELECT {COLUMNS} FROM lanes WHERE lead_thread_id = ?1 AND (?2 IS NULL OR policy = ?2) \
         AND {LIVE} ORDER BY last_activity_at_ns DESC, lane_id LIMIT ?3"
    ))?;
    let lanes = statement
        .query_map(params![request.lead_thread_id, policy, fetch], from_row)?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let positioned = lanes
        .into_iter()
        .enumerate()
        .map(|(index, lane)| (i64::try_from(index).unwrap_or(i64::MAX), lane))
        .collect();
    let (items, next) = fill_page(tx, positioned, limit, as_of)?;
    let Some(next) = next else {
        return Ok(LaneListPage {
            items,
            next_cursor: None,
            has_more: false,
            snapshot_revision: as_of,
        });
    };
    // More than one page: freeze the order so the later pages continue this listing.
    tx.execute(
        "INSERT INTO lane_list_snapshots (filter_hash, lead_thread_id, as_of_revision) \
         VALUES (?1, ?2, ?3)",
        params![
            filter,
            request.lead_thread_id,
            to_i64(as_of, "lane ledger revision")?
        ],
    )?;
    let snapshot_id = tx.last_insert_rowid();
    tx.execute(
        &format!(
            "INSERT INTO lane_list_snapshot_rows (snapshot_id, position, lane_id) \
             SELECT ?1, row_number() OVER (ORDER BY last_activity_at_ns DESC, lane_id) - 1, lane_id \
             FROM lanes WHERE lead_thread_id = ?2 AND (?3 IS NULL OR policy = ?3) AND {LIVE}"
        ),
        params![snapshot_id, request.lead_thread_id, policy],
    )?;
    evict_snapshots(tx, &request.lead_thread_id)?;
    Ok(LaneListPage {
        items,
        next_cursor: Some(encode_cursor(snapshot_id, next, filter)),
        has_more: true,
        snapshot_revision: as_of,
    })
}

/// Keeps the newest [`LANE_LIST_SNAPSHOTS_PER_LEAD`] listings of the lead and the
/// newest [`LANE_LIST_SNAPSHOTS_MAX`] overall. Bounded by count, never by time: a
/// cursor of an evicted listing is `stale_cursor`.
fn evict_snapshots(tx: &Transaction<'_>, lead_thread_id: &str) -> Result<(), StorageError> {
    tx.execute(
        "DELETE FROM lane_list_snapshots WHERE lead_thread_id = ?1 AND snapshot_id NOT IN \
         (SELECT snapshot_id FROM lane_list_snapshots WHERE lead_thread_id = ?1 \
          ORDER BY snapshot_id DESC LIMIT ?2)",
        params![
            lead_thread_id,
            i64::try_from(LANE_LIST_SNAPSHOTS_PER_LEAD).unwrap_or(i64::MAX)
        ],
    )?;
    tx.execute(
        "DELETE FROM lane_list_snapshots WHERE snapshot_id NOT IN \
         (SELECT snapshot_id FROM lane_list_snapshots ORDER BY snapshot_id DESC LIMIT ?1)",
        [i64::try_from(LANE_LIST_SNAPSHOTS_MAX).unwrap_or(i64::MAX)],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lanes::{LaneScope, LaneWorkspaceDeclaration};

    fn record(n: u32, notes: usize) -> LaneRecord {
        LaneRecord {
            lane_id: format!("lane-{n}"),
            thread_id: format!("thread-{n}"),
            origin_thread_id: "lead".into(),
            origin_run_id: "run".into(),
            creation_key: format!("k{n}"),
            scope: LaneScope::Lane,
            lead_thread_id: Some("lead".into()),
            policy: LanePolicy::Attached,
            management_revision: 1,
            detached_since_management_revision: None,
            brief: LaneBrief {
                verification_requirements: Default::default(),
                objective: "o".into(),
                inputs: vec![],
                deliverables: vec![],
                acceptance_criteria: vec!["c".into()],
                constraints: vec![],
                workspace: LaneWorkspaceDeclaration {
                    branch: None,
                    worktree: None,
                    notes: Some("n".repeat(notes)),
                },
                report_policy: LanePolicy::Attached,
            },
            brief_revision: 1,
            start_seed: String::new(),
            start_blocked_reason: None,
            root_run_id: None,
            generation: 0,
            latest_report_seq: 0,
            created_at_ns: 1,
            updated_at_ns: 1,
            last_activity_at_ns: 1,
        }
    }

    #[test]
    fn a_single_entry_larger_than_a_page_is_an_error_never_returned_anyway() {
        let conn = crate::Database::open_in_memory().unwrap();
        let rows = vec![(0, record(1, LANE_LIST_PAGE_MAX_BYTES + 1))];
        let error = fill_page(&conn.conn, rows, 20, 1).unwrap_err();
        assert!(matches!(
            error,
            StorageError::LaneInvalid(ref e) if e.field == "lane_list_item"
        ));
    }

    #[test]
    fn entries_are_counted_with_the_cursor_and_fixed_fields_of_the_whole_page() {
        let conn = crate::Database::open_in_memory().unwrap();
        // Three entries of about 6 KiB each: two fit under 16 KiB with the page's own
        // structure and cursor, the third does not.
        let rows = (0..3).map(|n| (i64::from(n), record(n, 6_000))).collect();
        let (items, next) = fill_page(&conn.conn, rows, 20, 1).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(next, Some(2));
        let page = LaneListPage {
            items,
            next_cursor: Some(worst_cursor()),
            has_more: true,
            snapshot_revision: 1,
        };
        assert!(serde_json::to_vec(&page).unwrap().len() <= LANE_LIST_PAGE_MAX_BYTES);
    }
}
