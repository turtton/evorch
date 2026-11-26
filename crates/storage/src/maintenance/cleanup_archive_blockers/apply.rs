//! Applying a saved plan: every exclusion first, then a verified backup, then one transaction.

use std::path::PathBuf;
use std::time::SystemTime;

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::Serialize;

use super::super::invalid;
use super::super::offline::{enable_foreign_keys, lease_file, require_no_foreign_key_violations};
use super::backup::ensure_backup;
use super::{
    CleanupPlan, Manifest, PLAN_KIND, PLAN_VERSION, SidebarBindings, enumerate, identity_of,
    path_text, pending_question_notifications, plan_hash_of, require_current_schema, sha256_file,
};
use crate::StorageError;
use crate::lane_control::{CloseLane, LaneCloseKind, LaneCloseOutcome, LaneCloser};

/// A plan file larger than this is not a plan.
const MAX_PLAN_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct CleanupApplyOptions {
    pub db: PathBuf,
    pub config_dir: PathBuf,
    pub sidebar: PathBuf,
    pub plan: PathBuf,
    pub backup: PathBuf,
}

#[derive(Debug, Serialize)]
pub struct CleanupApplySummary {
    /// `applied` or `already_applied`.
    pub status: &'static str,
    pub plan_hash: String,
    pub backup: Option<PathBuf>,
    pub backup_reused: bool,
    pub cancelled_questions: usize,
    pub obsoleted_notifications: usize,
    pub closed_lanes: usize,
    pub checkpoint_truncated: bool,
    pub message: String,
}

/// Where a test makes the apply fail, to prove every earlier step rolls back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(super) enum Fault {
    None,
    AfterBackup,
    AfterQuestions,
    AfterNotifications,
    AfterLanes,
    BeforeCommit,
    AfterCommit,
    Checkpoint,
}

impl Fault {
    fn hit(self, at: Fault) -> Result<(), StorageError> {
        if self == at {
            return Err(invalid(format!("injected failure {at:?}")));
        }
        Ok(())
    }
}

fn read_plan(path: &std::path::Path) -> Result<CleanupPlan, StorageError> {
    let meta = std::fs::metadata(path).map_err(|e| StorageError::Io(e.to_string()))?;
    if !meta.is_file() || meta.len() > MAX_PLAN_BYTES {
        return Err(invalid("the plan is not a regular file of a sane size"));
    }
    let bytes = std::fs::read(path).map_err(|e| StorageError::Io(e.to_string()))?;
    serde_json::from_slice(&bytes).map_err(|e| invalid(format!("the plan cannot be read: {e}")))
}

/// The plan must be internally consistent and name this database and configuration: a
/// hand-edited plan, or one written for another installation, never gets near the data.
fn check_plan(plan: &CleanupPlan, db: &str, config: &str) -> Result<(), StorageError> {
    if plan.plan_kind != PLAN_KIND || plan.plan_version != PLAN_VERSION {
        return Err(invalid("the plan has an unknown kind or version"));
    }
    if plan.canonical_db_path != db || plan.canonical_config_path != config {
        return Err(invalid(
            "the plan was made for another database or configuration",
        ));
    }
    if plan.target_identity != identity_of(db, config) {
        return Err(invalid("the plan identity does not match its paths"));
    }
    if plan.manifest.sha256()? != plan.target_manifest_sha256 {
        return Err(invalid(
            "the plan manifest does not match its hash: it was edited",
        ));
    }
    if plan_hash_of(
        &plan.target_identity,
        &plan.source_image_sha256,
        &plan.target_manifest_sha256,
    ) != plan.plan_hash
    {
        return Err(invalid("the plan hash does not match its contents"));
    }
    let manifest = &plan.manifest;
    if plan.schema_version != manifest.schema_version
        || plan.counts.questions_to_cancel != manifest.questions.len()
        || plan.counts.lanes_to_discard != manifest.lanes.len()
        || plan.counts.question_notifications_to_obsolete != manifest.notifications.len()
        || plan.apply_allowed != manifest.unprocessable.is_empty()
    {
        return Err(invalid("the plan summary does not match its manifest"));
    }
    if !plan.apply_allowed {
        return Err(invalid(
            "the plan lists items that cannot be processed; nothing is applied",
        ));
    }
    Ok(())
}

/// A non-empty WAL holds committed work this command must not read around or recover.
fn require_empty_wal(db: &std::path::Path) -> Result<(), StorageError> {
    let wal = crate::db::suffixed_path(db, "-wal");
    match std::fs::metadata(&wal) {
        Ok(meta) if meta.len() != 0 => Err(invalid(
            "the database has a non-empty WAL: start evorch once and exit cleanly to checkpoint it, then preview again (the WAL and lock files must never be deleted by hand)",
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(StorageError::Io(error.to_string())),
    }
}

struct Receipt {
    manifest_sha256: String,
    source_sha256: String,
    cancelled: i64,
    obsoleted: i64,
    closed: i64,
}

fn read_receipt(conn: &Connection, plan_hash: &str) -> Result<Option<Receipt>, StorageError> {
    Ok(conn
        .query_row(
            "SELECT manifest_sha256, source_sha256, cancelled_questions, obsoleted_notifications, closed_lanes \
             FROM archive_blocker_cleanup_receipts WHERE plan_hash=?1",
            [plan_hash],
            |row| {
                Ok(Receipt {
                    manifest_sha256: row.get(0)?,
                    source_sha256: row.get(1)?,
                    cancelled: row.get(2)?,
                    obsoleted: row.get(3)?,
                    closed: row.get(4)?,
                })
            },
        )
        .optional()?)
}

/// Applies `plan` to the database.
///
/// The caller holds the configuration's launch and lifetime locks for the whole call (the
/// storage crate cannot take them); this function holds the database maintenance lease and
/// the SQLite exclusive lock.
///
/// # Errors
/// Any difference between the plan and the database, a non-empty WAL, a missing or
/// mismatching backup, or a failure while changing data (which rolls everything back).
pub fn apply_archive_blocker_cleanup(
    options: &CleanupApplyOptions,
) -> Result<CleanupApplySummary, StorageError> {
    apply_with(options, Fault::None)
}

pub(super) fn apply_with(
    options: &CleanupApplyOptions,
    fault: Fault,
) -> Result<CleanupApplySummary, StorageError> {
    let io = |error: std::io::Error| StorageError::Io(error.to_string());
    let db = options.db.canonicalize().map_err(io)?;
    let config = options.config_dir.canonicalize().map_err(io)?;
    let plan = read_plan(&options.plan)?;
    check_plan(&plan, &path_text(&db)?, &path_text(&config)?)?;

    let lease = lease_file(&db)?;
    lease.try_lock().map_err(|error| {
        invalid(format!(
            "database is in use; stop all evorch processes: {error}"
        ))
    })?;
    require_empty_wal(&db)?;
    let conn = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    enable_foreign_keys(&conn)?;
    conn.busy_timeout(std::time::Duration::ZERO)?;
    conn.pragma_update(None, "locking_mode", "EXCLUSIVE")?;
    conn.execute_batch("BEGIN EXCLUSIVE; COMMIT;")
        .map_err(|error| {
            invalid(format!(
                "database is in use; exclusive SQLite lock refused: {error}"
            ))
        })?;
    // A writer that ignores the lease could have committed between the check above and the
    // exclusive lock. Looked at again, the WAL must still be empty.
    require_empty_wal(&db)?;
    let version = require_current_schema(&conn)?;
    if version != plan.schema_version {
        return Err(invalid("the plan was made for another schema version"));
    }

    if let Some(receipt) = read_receipt(&conn, &plan.plan_hash)? {
        let matches = receipt.manifest_sha256 == plan.target_manifest_sha256
            && receipt.source_sha256 == plan.source_image_sha256
            && receipt.cancelled == plan.counts.questions_to_cancel as i64
            && receipt.obsoleted == plan.counts.question_notifications_to_obsolete as i64
            && receipt.closed == plan.counts.lanes_to_discard as i64;
        if !matches {
            return Err(invalid(
                "a completion receipt exists for this plan hash but its contents differ from the plan",
            ));
        }
        return Ok(CleanupApplySummary {
            status: "already_applied",
            plan_hash: plan.plan_hash,
            backup: None,
            backup_reused: false,
            cancelled_questions: plan.counts.questions_to_cancel,
            obsoleted_notifications: plan.counts.question_notifications_to_obsolete,
            closed_lanes: plan.counts.lanes_to_discard,
            checkpoint_truncated: true,
            message: "this plan was already applied; nothing was changed".into(),
        });
    }

    if sha256_file(&db)? != plan.source_image_sha256 {
        return Err(invalid(
            "the database is not the image the plan was made from (it changed since the preview): preview again",
        ));
    }
    let sidebar = SidebarBindings::read(&options.sidebar);
    if enumerate(&conn, &sidebar)? != plan.manifest {
        return Err(invalid("the targets differ from the plan: preview again"));
    }

    let backup = ensure_backup(&db, &plan.source_image_sha256, &options.backup)?;
    fault.hit(Fault::AfterBackup)?;

    let mut summary = transact(&conn, &plan, &options.sidebar, &backup.sha256, fault)?;
    summary.backup = Some(backup.path);
    summary.backup_reused = backup.reused;
    fault.hit(Fault::AfterCommit)?;
    const COMMITTED: &str = "the cleanup is COMMITTED (its receipt is stored) but the WAL checkpoint is incomplete: start evorch once, exit it cleanly, then rerun the same apply command to confirm already_applied";
    // Any failure of the checkpoint (a busy reader or an SQL/I/O error) is reported as
    // committed: the receipt is stored, so the same plan confirms it on a rerun.
    let checkpoint = fault
        .hit(Fault::Checkpoint)
        .map_err(|error| error.to_string())
        .and_then(|()| {
            conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                row.get::<_, i64>(0)
            })
            .map_err(|error| error.to_string())
        });
    match checkpoint {
        Ok(0) => {}
        Ok(_) => return Err(invalid(COMMITTED)),
        Err(error) => return Err(invalid(format!("{COMMITTED} ({error})"))),
    }
    summary.checkpoint_truncated = true;
    summary.message = "applied and checkpointed".into();
    Ok(summary)
}

fn transact(
    conn: &Connection,
    plan: &CleanupPlan,
    sidebar_path: &std::path::Path,
    backup_sha256: &str,
    fault: Fault,
) -> Result<CleanupApplySummary, StorageError> {
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    // The decisive comparison: the same read, inside the transaction that changes the data.
    let sidebar = SidebarBindings::read(sidebar_path);
    let manifest: &Manifest = &plan.manifest;
    if &enumerate(&tx, &sidebar)? != manifest {
        return Err(invalid("the targets differ from the plan: preview again"));
    }
    for question in &manifest.questions {
        let changed = tx.execute(
            "UPDATE user_questions SET cancelled=1, payload=json_set(payload,'$.cancelled',json('true')) \
             WHERE id=?1 AND answered=0 AND cancelled=0",
            [&question.id],
        )?;
        if changed != 1 {
            return Err(invalid(format!(
                "question {} could not be cancelled",
                question.id
            )));
        }
    }
    fault.hit(Fault::AfterQuestions)?;
    for notification in &manifest.notifications {
        let changed = tx.execute(
            "UPDATE lane_notifications SET state='obsolete' WHERE notification_id=?1 AND state='pending'",
            [&notification.notification_id],
        )?;
        if changed != 1 {
            return Err(invalid(format!(
                "notification {} could not be made obsolete",
                notification.notification_id
            )));
        }
    }
    fault.hit(Fault::AfterNotifications)?;
    let at_ns = crate::system_time_to_ns(SystemTime::now())?;
    let note = format!("archive-blocker cleanup {}", &plan.plan_hash[..16]);
    for lane in &manifest.lanes {
        let result = crate::repo::lane_control::close_lane_in(
            &tx,
            CloseLane {
                closer: LaneCloser::Human {
                    surface: "maintenance_cli".into(),
                    client_id: None,
                },
                kind: LaneCloseKind::Discarded,
                lane_id: lane.lane_id.clone(),
                ready_seq: None,
                expected_management_revision: lane.management_revision,
                expected_request_revision: lane.request_revision,
                note: note.clone(),
                verification_id: None,
                at_ns,
            },
        )?;
        if result.outcome != LaneCloseOutcome::Closed {
            return Err(invalid(format!(
                "lane {} was not closed by this plan",
                lane.lane_id
            )));
        }
    }
    fault.hit(Fault::AfterLanes)?;
    verify(&tx, manifest)?;
    tx.execute(
        "INSERT INTO archive_blocker_cleanup_receipts(plan_hash,manifest_sha256,source_sha256,backup_sha256,cancelled_questions,obsoleted_notifications,closed_lanes,completed_at_ns) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        params![
            plan.plan_hash,
            plan.target_manifest_sha256,
            plan.source_image_sha256,
            backup_sha256,
            manifest.questions.len() as i64,
            manifest.notifications.len() as i64,
            manifest.lanes.len() as i64,
            at_ns,
        ],
    )?;
    fault.hit(Fault::BeforeCommit)?;
    tx.commit()?;
    Ok(CleanupApplySummary {
        status: "applied",
        plan_hash: plan.plan_hash.clone(),
        backup: None,
        backup_reused: false,
        cancelled_questions: manifest.questions.len(),
        obsoleted_notifications: manifest.notifications.len(),
        closed_lanes: manifest.lanes.len(),
        checkpoint_truncated: false,
        message: "committed".into(),
    })
}

fn verify(tx: &Connection, manifest: &Manifest) -> Result<(), StorageError> {
    require_no_foreign_key_violations(tx)?;
    for question in &manifest.questions {
        let payload: String = tx.query_row(
            "SELECT payload FROM user_questions WHERE id=?1 AND cancelled=1 AND answered=0",
            [&question.id],
            |row| row.get(0),
        )?;
        let stored: event_bus::UserQuestion = serde_json::from_str(&payload)
            .map_err(|e| invalid(format!("cancelled question unreadable: {e}")))?;
        if stored.status() != event_bus::QuestionStatus::Cancelled || stored.answer_origin.is_some()
        {
            return Err(invalid(format!(
                "question {} is not cleanly cancelled",
                question.id
            )));
        }
    }
    let remaining = pending_question_notifications(tx)?;
    for question in &manifest.questions {
        if remaining.contains_key(&question.id) {
            return Err(invalid(format!(
                "a pending question notification remains for cancelled question {}",
                question.id
            )));
        }
    }
    for lane in &manifest.lanes {
        let discarded: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM lanes l JOIN lane_closes c ON c.close_id=l.current_close_id AND c.lane_id=l.lane_id \
             WHERE l.lane_id=?1 AND c.kind='discarded')",
            [&lane.lane_id],
            |row| row.get(0),
        )?;
        if !discarded {
            return Err(invalid(format!(
                "lane {} has no discarded close",
                lane.lane_id
            )));
        }
    }
    Ok(())
}
