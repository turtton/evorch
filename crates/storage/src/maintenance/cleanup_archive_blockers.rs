//! One-time offline cleanup of the two things that keep a thread from being archived by
//! the automatic retention: questions nobody answered and managed lanes nobody closed.
//!
//! * Every pending question becomes **cancelled**: a terminal state beside answered. No
//!   answer, answer origin or approval is recorded, and the question's pending lane
//!   notifications become obsolete in the same transaction so nothing wakes a lead for it.
//! * Every attached or summary lane without a valid close is closed as **discarded** through
//!   the ordinary close transition. Nothing is stopped and no run is touched.
//!
//! `preview` reads the database read-only and prints a plan whose manifest fixes exactly
//! what `apply` may change. `apply` refuses anything that differs from the plan, takes a
//! verified backup first and changes everything in one transaction.

mod apply;
mod backup;
mod sidebar;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::invalid;
use super::offline::{preview_lease, read_only_preview_connection};
use crate::StorageError;
pub use apply::{CleanupApplyOptions, CleanupApplySummary, apply_archive_blocker_cleanup};
pub use sidebar::SidebarBindings;

/// Version of the plan layout; a plan of another version is refused.
pub const PLAN_VERSION: u32 = 1;
const PLAN_KIND: &str = "cleanup-archive-blockers";

/// Where the cleanup looks. `config_dir` and `sidebar` belong to the same installation as `db`.
#[derive(Debug, Clone)]
pub struct CleanupPreviewOptions {
    pub db: PathBuf,
    pub config_dir: PathBuf,
    pub sidebar: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuestionTarget {
    pub id: String,
    pub title: String,
    pub requester_run_id: String,
    pub root_run_id: String,
    pub recipient_run_ids: Vec<String>,
    pub blocking: bool,
    /// `user`, `lead:<thread>` or `legacy` (no canonical route).
    pub responder: String,
    pub routing_revision: u64,
    pub payload_sha256: String,
    pub pending_notification_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneTarget {
    pub lane_id: String,
    pub thread_id: String,
    pub lead_thread_id: String,
    pub policy: String,
    pub management_revision: u64,
    pub request_revision: u64,
    pub latest_report_seq: u64,
    pub latest_report_status: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotificationTarget {
    pub notification_id: String,
    pub question_id: String,
    pub lane_id: String,
    pub destination_thread_id: String,
    pub claimed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Exclusion {
    pub kind: String,
    pub id: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Unprocessable {
    pub kind: String,
    pub id: String,
    pub reason: String,
}

/// Which live threads still bind a run of a question that the fence would reject.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunBindingEvidence {
    pub run_id: String,
    pub database_live_threads: Vec<String>,
    pub sidebar_live_threads: Vec<String>,
}

/// Everything `apply` may change, and the reasons for everything it leaves alone.
/// Sorted, so the same database state always gives the same hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema_version: u32,
    pub questions: Vec<QuestionTarget>,
    pub lanes: Vec<LaneTarget>,
    pub notifications: Vec<NotificationTarget>,
    pub exclusions: Vec<Exclusion>,
    pub unprocessable: Vec<Unprocessable>,
    pub run_bindings: Vec<RunBindingEvidence>,
}

impl Manifest {
    pub fn sha256(&self) -> Result<String, StorageError> {
        let bytes = serde_json::to_vec(self).map_err(|e| invalid(e.to_string()))?;
        Ok(hex(&Sha256::digest(bytes)))
    }

    /// Leads that receive a new Closed notification, counted once each.
    pub fn closed_notification_recipients(&self) -> usize {
        self.lanes
            .iter()
            .map(|lane| lane.lead_thread_id.as_str())
            .collect::<BTreeSet<_>>()
            .len()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanCounts {
    pub questions_to_cancel: usize,
    pub question_notifications_to_obsolete: usize,
    pub lanes_to_discard: usize,
    pub closed_notification_recipients: usize,
    pub excluded: usize,
    pub unprocessable: usize,
}

/// The preview output and the input of `apply`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CleanupPlan {
    pub plan_kind: String,
    pub plan_version: u32,
    pub canonical_db_path: String,
    pub canonical_config_path: String,
    pub schema_version: u32,
    pub target_identity: String,
    /// SHA-256 of the checkpointed main database file.
    pub source_image_sha256: String,
    /// SHA-256 of the manifest alone, independent of the rest of the database image.
    pub target_manifest_sha256: String,
    /// Names this operation: identity, image, manifest and version. Not an authorization.
    pub plan_hash: String,
    pub apply_allowed: bool,
    pub counts: PlanCounts,
    pub manifest: Manifest,
    pub notices: Vec<String>,
}

pub(super) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

pub(super) fn sha256_file(path: &Path) -> Result<String, StorageError> {
    let io = |error: std::io::Error| StorageError::Io(error.to_string());
    let mut file = File::open(path).map_err(io)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = file.read(&mut buffer).map_err(io)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex(&hasher.finalize()))
}

pub(super) fn identity_of(db: &str, config: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(db.as_bytes());
    hasher.update([0]);
    hasher.update(config.as_bytes());
    hex(&hasher.finalize())
}

pub(super) fn plan_hash_of(
    target_identity: &str,
    source_image_sha256: &str,
    target_manifest_sha256: &str,
) -> String {
    let mut hasher = Sha256::new();
    for part in [
        PLAN_KIND,
        &PLAN_VERSION.to_string(),
        target_identity,
        source_image_sha256,
        target_manifest_sha256,
    ] {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    hex(&hasher.finalize())
}

fn notices() -> Vec<String> {
    vec![
        "A cancelled question is not an answer or an approval; nothing is recorded as answered.".into(),
        "Discarding a lane closes its ledger entry; it does not stop any run or touch its workspace.".into(),
        "Other holds (pins, goals, recent activity, unresolved families) are not released.".into(),
        "Once these holds are released, a thread can be archived automatically and, with the default policy, deleted 7 days later.".into(),
        "A preview is not permission to apply. Start nothing between the preview and the apply: starting evorch changes the database and requires a new preview.".into(),
    ]
}

/// The schema this binary writes; the cleanup works on nothing else.
pub(super) fn require_current_schema(conn: &Connection) -> Result<u32, StorageError> {
    let version: u32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let current = crate::migrations::MIGRATIONS.len() as u32;
    if version < current {
        return Err(invalid(format!(
            "database schema is version {version}, this evorch needs {current}: start the current evorch once, exit it cleanly, then preview again (the cleanup never migrates)"
        )));
    }
    if version > current {
        return Err(invalid(format!(
            "database schema is version {version}, newer than this evorch ({current}); use the evorch that wrote it"
        )));
    }
    Ok(version)
}

/// Reads the plan's subject read-only and returns the plan. Nothing is created or changed.
///
/// # Errors
/// The database is in use, has a non-empty WAL, is not at the current schema, or a file
/// changed while it was read.
pub fn preview_archive_blocker_cleanup(
    options: &CleanupPreviewOptions,
) -> Result<CleanupPlan, StorageError> {
    let _lease = preview_lease(&options.db)?;
    let (conn, files) = read_only_preview_connection(&options.db)?;
    let schema_version = require_current_schema(&conn)?;
    let db = options
        .db
        .canonicalize()
        .map_err(|error| StorageError::Io(error.to_string()))?;
    let config = options
        .config_dir
        .canonicalize()
        .map_err(|error| StorageError::Io(error.to_string()))?;
    let canonical_db_path = path_text(&db)?;
    let canonical_config_path = path_text(&config)?;
    let source_image_sha256 = sha256_file(&db)?;
    let sidebar = SidebarBindings::read(&options.sidebar);
    let manifest = enumerate(&conn, &sidebar)?;
    files.verify()?;
    if sha256_file(&db)? != source_image_sha256 {
        return Err(invalid(
            "database changed while the preview was read; stop all evorch processes and retry",
        ));
    }
    build_plan(
        manifest,
        schema_version,
        canonical_db_path,
        canonical_config_path,
        source_image_sha256,
    )
}

pub(super) fn path_text(path: &Path) -> Result<String, StorageError> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| invalid("paths must be UTF-8"))
}

fn build_plan(
    manifest: Manifest,
    schema_version: u32,
    canonical_db_path: String,
    canonical_config_path: String,
    source_image_sha256: String,
) -> Result<CleanupPlan, StorageError> {
    let target_manifest_sha256 = manifest.sha256()?;
    let target_identity = identity_of(&canonical_db_path, &canonical_config_path);
    let plan_hash = plan_hash_of(
        &target_identity,
        &source_image_sha256,
        &target_manifest_sha256,
    );
    let counts = PlanCounts {
        questions_to_cancel: manifest.questions.len(),
        question_notifications_to_obsolete: manifest.notifications.len(),
        lanes_to_discard: manifest.lanes.len(),
        closed_notification_recipients: manifest.closed_notification_recipients(),
        excluded: manifest.exclusions.len(),
        unprocessable: manifest.unprocessable.len(),
    };
    Ok(CleanupPlan {
        plan_kind: PLAN_KIND.into(),
        plan_version: PLAN_VERSION,
        canonical_db_path,
        canonical_config_path,
        schema_version,
        target_identity,
        source_image_sha256,
        target_manifest_sha256,
        plan_hash,
        apply_allowed: manifest.unprocessable.is_empty(),
        counts,
        manifest,
        notices: notices(),
    })
}

fn unprocessable(kind: &str, id: &str, reason: impl Into<String>) -> Unprocessable {
    Unprocessable {
        kind: kind.into(),
        id: id.into(),
        reason: reason.into(),
    }
}

/// Lists everything the cleanup would change or leave, from stored rows only. Never
/// truncates: a long list is a long plan.
pub(super) fn enumerate(
    conn: &Connection,
    sidebar: &Result<SidebarBindings, String>,
) -> Result<Manifest, StorageError> {
    let schema_version: u32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let mut manifest = Manifest {
        schema_version,
        questions: Vec::new(),
        lanes: Vec::new(),
        notifications: Vec::new(),
        exclusions: Vec::new(),
        unprocessable: Vec::new(),
        run_bindings: Vec::new(),
    };
    if let Err(reason) = sidebar {
        // The sidebar is part of the evidence for every remnant decision; without it
        // nothing is assumed.
        manifest
            .unprocessable
            .push(unprocessable("sidebar", "sidebar.json", reason.clone()));
    }
    enumerate_questions(conn, sidebar, &mut manifest)?;
    enumerate_lanes(conn, &mut manifest)?;
    manifest.questions.sort_by(|a, b| a.id.cmp(&b.id));
    manifest.lanes.sort_by(|a, b| a.lane_id.cmp(&b.lane_id));
    manifest
        .notifications
        .sort_by(|a, b| a.notification_id.cmp(&b.notification_id));
    manifest
        .exclusions
        .sort_by(|a, b| (&a.kind, &a.id).cmp(&(&b.kind, &b.id)));
    manifest
        .unprocessable
        .sort_by(|a, b| (&a.kind, &a.id).cmp(&(&b.kind, &b.id)));
    manifest
        .run_bindings
        .sort_by(|a, b| a.run_id.cmp(&b.run_id));
    Ok(manifest)
}

fn tombstoned_runs(conn: &Connection) -> Result<BTreeSet<String>, StorageError> {
    let mut statement = conn.prepare("SELECT run_id FROM run_tombstones")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn tombstoned_threads(conn: &Connection) -> Result<BTreeSet<String>, StorageError> {
    let mut statement = conn.prepare("SELECT thread_id FROM thread_tombstones")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn database_live_threads(
    conn: &Connection,
    run: &str,
    dead_threads: &BTreeSet<String>,
) -> Result<Vec<String>, StorageError> {
    let mut statement = conn.prepare(
        "SELECT thread_id FROM thread_runs WHERE run_id=?1 \
         UNION SELECT thread_id FROM threads WHERE root_run_id=?1",
    )?;
    let rows = statement.query_map([run], |row| row.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        let thread = row?;
        if !dead_threads.contains(&thread) {
            out.push(thread);
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// Pending question notifications by question ID, from one scan.
fn pending_question_notifications(
    conn: &Connection,
) -> Result<BTreeMap<String, Vec<(NotificationTarget, String)>>, StorageError> {
    let mut statement = conn.prepare(
        "SELECT notification_id, lane_id, source_thread_id, destination_thread_id, claim_epoch, \
         CASE WHEN json_valid(payload) THEN json_extract(payload,'$.kind.question_id') END \
         FROM lane_notifications WHERE event_kind='question' AND state='pending' AND source_kind='lane'",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Option<i64>>(4)?,
            row.get::<_, Option<String>>(5)?,
        ))
    })?;
    let mut out: BTreeMap<String, Vec<(NotificationTarget, String)>> = BTreeMap::new();
    for row in rows {
        let (notification_id, lane_id, source, destination, claim, question) = row?;
        let (Some(question_id), Some(lane_id)) = (question, lane_id) else {
            continue;
        };
        out.entry(question_id.clone()).or_default().push((
            NotificationTarget {
                notification_id,
                question_id,
                lane_id,
                destination_thread_id: destination,
                claimed: claim.is_some(),
            },
            source,
        ));
    }
    Ok(out)
}

fn enumerate_questions(
    conn: &Connection,
    sidebar: &Result<SidebarBindings, String>,
    manifest: &mut Manifest,
) -> Result<(), StorageError> {
    let dead_runs = tombstoned_runs(conn)?;
    let dead_threads = tombstoned_threads(conn)?;
    let mut notifications = pending_question_notifications(conn)?;
    let lane_threads: BTreeMap<String, String> = {
        let mut statement = conn.prepare("SELECT lane_id, thread_id FROM lanes")?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<Result<_, _>>()?
    };
    let mut statement = conn.prepare(
        "SELECT id, run_id, root_run_id, payload, answered, cancelled FROM user_questions \
         WHERE answered=0 AND cancelled=0 ORDER BY id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    let stored: Vec<_> = rows.collect::<Result<_, _>>()?;
    let mut bindings: BTreeMap<String, RunBindingEvidence> = BTreeMap::new();
    for (id, run_id, root_run_id, payload) in stored {
        let question: event_bus::UserQuestion = match serde_json::from_str(&payload) {
            Ok(question) => question,
            Err(error) => {
                manifest.unprocessable.push(unprocessable(
                    "question",
                    &id,
                    format!("stored payload cannot be read: {error}"),
                ));
                continue;
            }
        };
        if question.id != id
            || question.answer.is_some()
            || question.answer_origin.is_some()
            || question.cancelled
        {
            manifest.unprocessable.push(unprocessable(
                "question",
                &id,
                "stored payload disagrees with the pending state of its row",
            ));
            continue;
        }
        let links: Vec<String> = {
            let mut statement = conn.prepare(
                "SELECT run_id FROM user_question_links WHERE question_id=?1 ORDER BY run_id",
            )?;
            let rows = statement.query_map([&id], |row| row.get::<_, String>(0))?;
            rows.collect::<Result<_, _>>()?
        };
        let route: Option<(Option<String>, u64)> = conn
            .query_row(
                "SELECT responder_thread_id, routing_revision FROM lane_question_routes WHERE question_id=?1",
                [&id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let (responder, routing_revision) = match route {
            None => ("legacy".to_owned(), 0),
            Some((None, revision)) => ("user".to_owned(), revision),
            Some((Some(thread), revision)) => (format!("lead:{thread}"), revision),
        };
        let notices = notifications.remove(&id).unwrap_or_default();

        // The v53 update fence: a question whose requester or root is deleted can only be
        // updated while a live recipient still holds it.
        let live_link = links.iter().any(|link| !dead_runs.contains(link));
        let owner_dead = dead_runs.contains(&run_id) || dead_runs.contains(&root_run_id);
        let fenced = owner_dead && !live_link;
        if fenced {
            let mut runs: BTreeSet<&String> = links.iter().collect();
            runs.insert(&run_id);
            runs.insert(&root_run_id);
            let mut still_bound = Vec::new();
            let mut all_dead = true;
            for run in runs {
                all_dead &= dead_runs.contains(run);
                let database = database_live_threads(conn, run, &dead_threads)?;
                let side = match sidebar {
                    Ok(bindings) => {
                        bindings.live_threads_binding(run, &|thread| dead_threads.contains(thread))
                    }
                    Err(_) => Vec::new(),
                };
                if !database.is_empty() || !side.is_empty() {
                    still_bound.push(run.clone());
                }
                bindings.insert(
                    run.clone(),
                    RunBindingEvidence {
                        run_id: run.clone(),
                        database_live_threads: database,
                        sidebar_live_threads: side,
                    },
                );
            }
            if sidebar.is_err() {
                manifest.unprocessable.push(unprocessable(
                    "question",
                    &id,
                    "fenced by a deleted run and the sidebar bindings cannot be read",
                ));
            } else if !all_dead || !still_bound.is_empty() {
                manifest.unprocessable.push(unprocessable(
                    "question",
                    &id,
                    format!(
                        "fenced by a deleted run but still held by live bindings or live runs ({})",
                        still_bound.join(",")
                    ),
                ));
            } else {
                manifest.exclusions.push(Exclusion {
                    kind: "question".into(),
                    id: id.clone(),
                    reason: "remnant: requester, root and every recipient run are deleted and no live thread binds them (database and sidebar)".into(),
                });
            }
            continue;
        }

        let mut blocked = false;
        for (notice, source) in &notices {
            let lane_thread = lane_threads.get(&notice.lane_id);
            let dead = dead_threads.contains(source)
                || dead_threads.contains(&notice.destination_thread_id)
                || lane_thread.is_some_and(|thread| dead_threads.contains(thread));
            if dead {
                manifest.unprocessable.push(unprocessable(
                    "notification",
                    &notice.notification_id,
                    format!(
                        "pending question notification of {id} cannot be made obsolete: a thread it names is deleted"
                    ),
                ));
                blocked = true;
            }
        }
        if blocked {
            continue;
        }
        let mut pending_ids: Vec<String> = notices
            .iter()
            .map(|(notice, _)| notice.notification_id.clone())
            .collect();
        pending_ids.sort();
        for (notice, _) in notices {
            manifest.notifications.push(notice);
        }
        manifest.questions.push(QuestionTarget {
            id,
            title: question.title.clone(),
            requester_run_id: run_id,
            root_run_id,
            recipient_run_ids: links,
            blocking: question.blocking,
            responder,
            routing_revision,
            payload_sha256: hex(&Sha256::digest(payload.as_bytes())),
            pending_notification_ids: pending_ids,
        });
    }
    manifest.run_bindings.extend(bindings.into_values());
    Ok(())
}

fn enumerate_lanes(conn: &Connection, manifest: &mut Manifest) -> Result<(), StorageError> {
    let dead_threads = tombstoned_threads(conn)?;
    let mut statement = conn.prepare(
        "SELECT l.lane_id, l.thread_id, l.lead_thread_id, l.policy, l.management_revision, \
         l.request_revision, l.latest_report_seq, \
         (SELECT r.status FROM lane_reports r WHERE r.lane_id=l.lane_id AND r.report_seq=l.latest_report_seq), \
         EXISTS(SELECT 1 FROM lane_closes c WHERE c.close_id=l.current_close_id AND c.lane_id=l.lane_id), \
         l.current_close_id IS NOT NULL \
         FROM lanes l ORDER BY l.lane_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            LaneTarget {
                lane_id: row.get(0)?,
                thread_id: row.get(1)?,
                lead_thread_id: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                policy: row.get(3)?,
                management_revision: row.get(4)?,
                request_revision: row.get(5)?,
                latest_report_seq: row.get(6)?,
                latest_report_status: row.get(7)?,
            },
            row.get::<_, bool>(8)?,
            row.get::<_, bool>(9)?,
        ))
    })?;
    let lanes: Vec<_> = rows.collect::<Result<_, _>>()?;
    for (lane, closed, has_pointer) in lanes {
        let exclude = |manifest: &mut Manifest, reason: &str| {
            manifest.exclusions.push(Exclusion {
                kind: "lane".into(),
                id: lane.lane_id.clone(),
                reason: reason.into(),
            });
        };
        if !matches!(lane.policy.as_str(), "attached" | "summary") {
            exclude(manifest, "detached lane: not a managed lane");
            continue;
        }
        if closed {
            exclude(manifest, "already closed by a valid close");
            continue;
        }
        if has_pointer {
            manifest.unprocessable.push(unprocessable(
                "lane",
                &lane.lane_id,
                "current close pointer names no stored close",
            ));
            continue;
        }
        if dead_threads.contains(&lane.thread_id) {
            exclude(
                manifest,
                "lane thread is deleted: the archive removes it from the family before deciding",
            );
            continue;
        }
        if lane.lead_thread_id.is_empty() || dead_threads.contains(&lane.lead_thread_id) {
            manifest.unprocessable.push(unprocessable(
                "lane",
                &lane.lane_id,
                "the lane is live but its lead thread is deleted or missing; a close cannot be recorded",
            ));
            continue;
        }
        manifest.lanes.push(lane);
    }
    Ok(())
}
