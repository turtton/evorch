//! Durable, secret-guarded improvement candidates with explicit per-project policy.
//!
//! Reads use [`Database`]; writes use [`crate::StorageHandle`] and the single writer.
//! Feature enablement and draft-file creation belong to callers, not storage.

use std::time::{Duration, SystemTime};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use crate::{Database, StorageError, ns_to_system_time, system_time_to_ns};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImprovementSource {
    Diagnostic,
    Lesson,
}

impl ImprovementSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Diagnostic => "diagnostic",
            Self::Lesson => "lesson",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, StorageError> {
        match value {
            "diagnostic" => Ok(Self::Diagnostic),
            "lesson" => Ok(Self::Lesson),
            _ => Err(StorageError::Serialization(format!(
                "invalid improvement source: {value}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImprovementSeverity {
    Info,
    Warning,
    Error,
}

impl ImprovementSeverity {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, StorageError> {
        match value {
            "info" => Ok(Self::Info),
            "warning" => Ok(Self::Warning),
            "error" => Ok(Self::Error),
            _ => Err(StorageError::Serialization(format!(
                "invalid improvement severity: {value}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImprovementStatus {
    New,
    Reviewed,
    Dismissed,
}

impl ImprovementStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Reviewed => "reviewed",
            Self::Dismissed => "dismissed",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, StorageError> {
        match value {
            "new" => Ok(Self::New),
            "reviewed" => Ok(Self::Reviewed),
            "dismissed" => Ok(Self::Dismissed),
            _ => Err(StorageError::Serialization(format!(
                "invalid improvement status: {value}"
            ))),
        }
    }

    pub const fn is_resolved(&self) -> bool {
        matches!(self, Self::Reviewed | Self::Dismissed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewImprovementCandidate {
    pub id: String,
    pub source: ImprovementSource,
    pub code: String,
    pub severity: ImprovementSeverity,
    pub title: String,
    /// Caller-bounded JSON/text; storage also enforces an absolute 64 KiB byte cap.
    pub evidence: String,
    pub dedup_key: String,
    pub run_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImprovementCandidate {
    pub id: String,
    pub project: String,
    pub created_at_ns: u64,
    pub source: ImprovementSource,
    pub code: String,
    pub severity: ImprovementSeverity,
    pub title: String,
    pub evidence: String,
    pub dedup_key: String,
    pub status: ImprovementStatus,
    /// Caller-supplied basename or relative path; storage does not create draft files.
    pub draft_path: Option<String>,
    pub run_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImprovementWritePolicy {
    pub duplicate_cooldown: Duration,
    /// Per-project limit on stored intakes since the current UTC day began.
    pub daily_limit: u32,
    /// Per-project retention cap; evict oldest resolved first, then oldest overall.
    pub max_candidates: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImprovementRecordOutcome {
    Stored { id: String },
    Duplicate { existing_id: String },
    RateLimited,
}

impl Database {
    /// List newest candidates first, optionally filtering by status; limit is capped at 1000.
    pub fn improvement_candidates(
        &self,
        project: &str,
        status: Option<ImprovementStatus>,
        limit: usize,
    ) -> Result<Vec<ImprovementCandidate>, StorageError> {
        let mut statement = self.conn.prepare(
            "SELECT candidate_id, project, created_at_ns, source, code, severity,
                    title, evidence, dedup_key, status, draft_path, run_id
             FROM improvement_candidates
             WHERE project = ?1 AND (?2 IS NULL OR status = ?2)
             ORDER BY created_at_ns DESC, rowid DESC LIMIT ?3",
        )?;
        let mut rows = statement.query(params![
            project,
            status.map(ImprovementStatus::as_str),
            limit.min(1000) as i64,
        ])?;
        let mut candidates = Vec::new();
        while let Some(row) = rows.next()? {
            candidates.push(candidate_row(row)?);
        }
        Ok(candidates)
    }

    pub fn improvement_candidate(
        &self,
        id: &str,
    ) -> Result<Option<ImprovementCandidate>, StorageError> {
        let mut statement = self.conn.prepare(
            "SELECT candidate_id, project, created_at_ns, source, code, severity,
                    title, evidence, dedup_key, status, draft_path, run_id
             FROM improvement_candidates WHERE candidate_id = ?1",
        )?;
        let mut rows = statement.query([id])?;
        rows.next()?.map(candidate_row).transpose()
    }
}

fn candidate_row(row: &rusqlite::Row<'_>) -> Result<ImprovementCandidate, StorageError> {
    let created_at_ns: i64 = row.get(2)?;
    Ok(ImprovementCandidate {
        id: row.get(0)?,
        project: row.get(1)?,
        created_at_ns: u64::try_from(created_at_ns)
            .map_err(|_| StorageError::OutOfRange("improvement created_at_ns"))?,
        source: ImprovementSource::parse(&row.get::<_, String>(3)?)?,
        code: row.get(4)?,
        severity: ImprovementSeverity::parse(&row.get::<_, String>(5)?)?,
        title: row.get(6)?,
        evidence: row.get(7)?,
        dedup_key: row.get(8)?,
        status: ImprovementStatus::parse(&row.get::<_, String>(9)?)?,
        draft_path: row.get(10)?,
        run_id: row.get(11)?,
    })
}

pub(crate) fn record(
    conn: &Connection,
    project: &str,
    candidate: &NewImprovementCandidate,
    policy: ImprovementWritePolicy,
) -> Result<ImprovementRecordOutcome, StorageError> {
    record_at(conn, project, candidate, policy, SystemTime::now())
}

fn record_at(
    conn: &Connection,
    project: &str,
    candidate: &NewImprovementCandidate,
    policy: ImprovementWritePolicy,
    now: SystemTime,
) -> Result<ImprovementRecordOutcome, StorageError> {
    let guard = crate::entity::SecretGuard::from_env();
    // Project is also caller-controlled persisted text, so guard it like lesson.project.
    for (field, text) in [
        ("project", project),
        ("id", candidate.id.as_str()),
        ("code", candidate.code.as_str()),
        ("title", candidate.title.as_str()),
        ("evidence", candidate.evidence.as_str()),
        ("dedup_key", candidate.dedup_key.as_str()),
    ] {
        guard.check_text("improvement", field, text)?;
        if text.trim().is_empty() {
            return Err(StorageError::Serialization(
                "improvement candidate fields must be non-empty".into(),
            ));
        }
    }
    if let Some(run_id) = &candidate.run_id {
        guard.check_text("improvement", "run_id", run_id)?;
    }
    if candidate.evidence.len() > 65_536 {
        return Err(StorageError::Serialization(
            "evidence exceeds 64 KiB storage cap".into(),
        ));
    }

    let now_ns = system_time_to_ns(now)?;
    // Lock before checking policy so separate storage instances cannot race these checks.
    let transaction = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let newest: Option<(String, i64)> = transaction
        .query_row(
            "SELECT candidate_id, created_at_ns FROM improvement_intake
             WHERE project = ?1 AND dedup_key = ?2 AND outcome = 'stored'
             ORDER BY seq DESC LIMIT 1",
            params![project, candidate.dedup_key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((existing_id, created_at_ns)) = newest {
        // A backwards clock jump must not bypass an active cooldown.
        let age = now
            .duration_since(ns_to_system_time(created_at_ns))
            .unwrap_or_default();
        if age < policy.duplicate_cooldown {
            return Ok(ImprovementRecordOutcome::Duplicate { existing_id });
        }
    }

    const DAY_NS: i64 = 86_400 * 1_000_000_000;
    let day_start_ns = now_ns - now_ns % DAY_NS;
    let today_count: i64 = transaction.query_row(
        "SELECT COUNT(*) FROM improvement_intake
         WHERE project = ?1 AND created_at_ns >= ?2 AND outcome = 'stored'",
        params![project, day_start_ns],
        |row| row.get(0),
    )?;
    if today_count >= i64::from(policy.daily_limit) {
        return Ok(ImprovementRecordOutcome::RateLimited);
    }

    transaction.execute(
        "INSERT INTO improvement_candidates
         (candidate_id, project, created_at_ns, source, code, severity,
          title, evidence, dedup_key, status, run_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'new', ?10)",
        params![
            candidate.id,
            project,
            now_ns,
            candidate.source.as_str(),
            candidate.code,
            candidate.severity.as_str(),
            candidate.title,
            candidate.evidence,
            candidate.dedup_key,
            candidate.run_id,
        ],
    )?;
    transaction.execute(
        "INSERT INTO improvement_intake
         (project, dedup_key, created_at_ns, outcome, candidate_id)
         VALUES (?1, ?2, ?3, 'stored', ?4)",
        params![project, candidate.dedup_key, now_ns, candidate.id],
    )?;

    let count: i64 = transaction.query_row(
        "SELECT COUNT(*) FROM improvement_candidates WHERE project = ?1",
        [project],
        |row| row.get(0),
    )?;
    let excess = count - i64::from(policy.max_candidates);
    if excess > 0 {
        // Equivalent to repeatedly deleting the oldest resolved, then oldest overall.
        // rowid breaks timestamp ties by insertion order.
        transaction.execute(
            "DELETE FROM improvement_candidates WHERE candidate_id IN (
                SELECT candidate_id FROM improvement_candidates WHERE project = ?1
                ORDER BY (status IN ('reviewed', 'dismissed')) DESC,
                         created_at_ns ASC, rowid ASC LIMIT ?2
             )",
            params![project, excess],
        )?;
    }
    transaction.commit()?;
    // Beyond the configured maximum 365-day cooldown. Prune after commit so a
    // housekeeping failure cannot roll back the candidate or its accounting.
    const INTAKE_RETENTION_NS: i64 = 400 * DAY_NS;
    let _ = conn.execute(
        "DELETE FROM improvement_intake WHERE created_at_ns < ?1",
        [now_ns - INTAKE_RETENTION_NS],
    );
    Ok(ImprovementRecordOutcome::Stored {
        id: candidate.id.clone(),
    })
}

pub(crate) fn set_status(
    conn: &Connection,
    id: &str,
    status: ImprovementStatus,
) -> Result<bool, StorageError> {
    Ok(conn.execute(
        "UPDATE improvement_candidates SET status = ?2 WHERE candidate_id = ?1",
        params![id, status.as_str()],
    )? > 0)
}

pub(crate) fn attach_draft(
    conn: &Connection,
    id: &str,
    draft_path: &str,
) -> Result<bool, StorageError> {
    crate::entity::SecretGuard::from_env().check_text("improvement", "draft_path", draft_path)?;
    if draft_path.trim().is_empty() {
        return Err(StorageError::Serialization(
            "improvement draft path must be non-empty".into(),
        ));
    }
    Ok(conn.execute(
        "UPDATE improvement_candidates SET draft_path = ?2 WHERE candidate_id = ?1",
        params![id, draft_path],
    )? > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Storage, StorageConfig};
    use std::time::UNIX_EPOCH;

    fn candidate(id: &str) -> NewImprovementCandidate {
        NewImprovementCandidate {
            id: id.into(),
            source: ImprovementSource::Diagnostic,
            code: "storage.busy".into(),
            severity: ImprovementSeverity::Warning,
            title: "Reduce writer contention".into(),
            evidence: r#"{"occurrences":3}"#.into(),
            dedup_key: format!("key-{id}"),
            run_id: Some("run-1".into()),
        }
    }

    fn policy() -> ImprovementWritePolicy {
        ImprovementWritePolicy {
            duplicate_cooldown: Duration::from_secs(60),
            daily_limit: 100,
            max_candidates: 100,
        }
    }

    fn stored(id: &str) -> ImprovementRecordOutcome {
        ImprovementRecordOutcome::Stored { id: id.into() }
    }

    fn at(seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(seconds)
    }

    fn record_test(db: &Database, id: &str, seconds: u64, policy: ImprovementWritePolicy) {
        assert_eq!(
            record_at(&db.conn, "p", &candidate(id), policy, at(seconds)).unwrap(),
            stored(id)
        );
    }

    fn ids(db: &Database, project: &str) -> Vec<String> {
        db.improvement_candidates(project, None, 1000)
            .unwrap()
            .into_iter()
            .map(|candidate| candidate.id)
            .collect()
    }

    fn intake_ids(db: &Database) -> Vec<String> {
        db.conn
            .prepare("SELECT candidate_id FROM improvement_intake ORDER BY seq")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    fn file_config(dir: &tempfile::TempDir) -> StorageConfig {
        StorageConfig {
            db_path: dir.path().join("improvements.db"),
            ..StorageConfig::default()
        }
    }

    #[test]
    fn in_memory_record_round_trips_all_fields() {
        let db = Database::open_in_memory().unwrap();
        let input = candidate("first");
        assert_eq!(
            record(&db.conn, "p", &input, policy()).unwrap(),
            stored("first")
        );
        let row = db.improvement_candidate("first").unwrap().unwrap();
        assert_eq!(row.id, input.id);
        assert_eq!(row.project, "p");
        assert!(row.created_at_ns > 0);
        assert_eq!(row.source, input.source);
        assert_eq!(row.code, input.code);
        assert_eq!(row.severity, input.severity);
        assert_eq!(row.title, input.title);
        assert_eq!(row.evidence, input.evidence);
        assert_eq!(row.dedup_key, input.dedup_key);
        assert_eq!(row.run_id, input.run_id);
        assert_eq!(row.status, ImprovementStatus::New);
        assert_eq!(row.draft_path, None);
        assert!(db.improvement_candidate("missing").unwrap().is_none());
    }

    #[test]
    fn dedup_uses_newest_project_row_and_zero_cooldown_disables_it() {
        let db = Database::open_in_memory().unwrap();
        record_test(&db, "first", 100, policy());
        let mut second = candidate("second");
        second.dedup_key = candidate("first").dedup_key;
        assert_eq!(
            record_at(&db.conn, "p", &second, policy(), at(101)).unwrap(),
            ImprovementRecordOutcome::Duplicate {
                existing_id: "first".into()
            }
        );
        let no_cooldown = ImprovementWritePolicy {
            duplicate_cooldown: Duration::ZERO,
            ..policy()
        };
        assert_eq!(
            record_at(&db.conn, "p", &second, no_cooldown, at(101)).unwrap(),
            stored("second")
        );
        second.id = "third".into();
        assert_eq!(
            record_at(&db.conn, "p", &second, policy(), at(160)).unwrap(),
            ImprovementRecordOutcome::Duplicate {
                existing_id: "second".into()
            }
        );
        assert_eq!(
            record_at(&db.conn, "other", &second, policy(), at(160)).unwrap(),
            stored("third")
        );
        assert_eq!(ids(&db, "p"), ["second", "first"]);
    }

    #[test]
    fn cooldown_expires_at_exact_boundary_and_handles_clock_rollback() {
        let db = Database::open_in_memory().unwrap();
        record_test(&db, "first", 100, policy());
        let mut second = candidate("second");
        second.dedup_key = candidate("first").dedup_key;
        for seconds in [99, 159] {
            assert!(matches!(
                record_at(&db.conn, "p", &second, policy(), at(seconds)).unwrap(),
                ImprovementRecordOutcome::Duplicate { .. }
            ));
        }
        assert_eq!(
            record_at(&db.conn, "p", &second, policy(), at(160)).unwrap(),
            stored("second")
        );
    }

    #[test]
    fn rate_limit_is_per_project_and_resets_at_utc_midnight() {
        let db = Database::open_in_memory().unwrap();
        let policy = ImprovementWritePolicy {
            daily_limit: 1,
            ..policy()
        };
        record_test(&db, "first", 86_399, policy);
        assert_eq!(
            record_at(&db.conn, "p", &candidate("second"), policy, at(86_399)).unwrap(),
            ImprovementRecordOutcome::RateLimited
        );
        assert_eq!(
            record_at(&db.conn, "other", &candidate("other"), policy, at(86_399)).unwrap(),
            stored("other")
        );
        record_test(&db, "second", 86_400, policy);
        assert_eq!(
            record_at(&db.conn, "p", &candidate("third"), policy, at(86_400)).unwrap(),
            ImprovementRecordOutcome::RateLimited
        );
        assert_eq!(ids(&db, "p"), ["second", "first"]);
    }

    #[test]
    fn dedup_precedes_rate_limit_and_zero_daily_limit_stores_nothing() {
        let db = Database::open_in_memory().unwrap();
        let zero = ImprovementWritePolicy {
            daily_limit: 0,
            ..policy()
        };
        assert_eq!(
            record_at(&db.conn, "p", &candidate("first"), zero, at(100)).unwrap(),
            ImprovementRecordOutcome::RateLimited
        );
        assert!(ids(&db, "p").is_empty());
        record_test(&db, "first", 100, policy());
        assert_eq!(
            record_at(&db.conn, "p", &candidate("first"), zero, at(101)).unwrap(),
            ImprovementRecordOutcome::Duplicate {
                existing_id: "first".into()
            }
        );
    }

    #[test]
    fn defaults_like_policy_preserves_normal_intake_behavior() {
        let db = Database::open_in_memory().unwrap();
        let normal = ImprovementWritePolicy {
            duplicate_cooldown: Duration::from_secs(86_400),
            daily_limit: 20,
            max_candidates: 200,
        };
        for index in 0..20 {
            let id = format!("candidate-{index}");
            record_test(&db, &id, 100, normal);
            assert_eq!(
                record_at(&db.conn, "p", &candidate(&id), normal, at(101)).unwrap(),
                ImprovementRecordOutcome::Duplicate { existing_id: id }
            );
        }
        assert_eq!(
            record_at(&db.conn, "p", &candidate("over-limit"), normal, at(101)).unwrap(),
            ImprovementRecordOutcome::RateLimited
        );
        assert_eq!(ids(&db, "p").len(), 20);
        assert_eq!(intake_ids(&db).len(), 20);
        // Daily accounting resets at midnight; yesterday's cooldown still applies.
        assert_eq!(
            record_at(&db.conn, "p", &candidate("candidate-0"), normal, at(86_400)).unwrap(),
            ImprovementRecordOutcome::Duplicate {
                existing_id: "candidate-0".into()
            }
        );
        record_test(&db, "next-day", 86_400, normal);
        let mut repeated = candidate("after-cooldown");
        repeated.dedup_key = candidate("candidate-0").dedup_key;
        assert_eq!(
            record_at(&db.conn, "p", &repeated, normal, at(86_500)).unwrap(),
            stored("after-cooldown")
        );
        assert_eq!(intake_ids(&db).len(), 22);
    }

    #[test]
    fn dedup_uses_latest_intake_sequence_after_clock_rollback() {
        let db = Database::open_in_memory().unwrap();
        record_test(&db, "first", 100, policy());
        let mut second = candidate("second");
        second.dedup_key = candidate("first").dedup_key;
        assert_eq!(
            record_at(
                &db.conn,
                "p",
                &second,
                ImprovementWritePolicy {
                    duplicate_cooldown: Duration::ZERO,
                    ..policy()
                },
                at(99),
            )
            .unwrap(),
            stored("second")
        );
        assert_eq!(
            record_at(&db.conn, "p", &second, policy(), at(101)).unwrap(),
            ImprovementRecordOutcome::Duplicate {
                existing_id: "second".into()
            }
        );
    }

    #[test]
    fn intake_housekeeping_prunes_only_rows_older_than_400_days() {
        let db = Database::open_in_memory().unwrap();
        let now = 401 * 86_400;
        let cutoff = now - 400 * 86_400;
        for (id, seconds) in [
            ("ancient", cutoff - 1),
            ("boundary", cutoff),
            ("recent", cutoff + 1),
        ] {
            db.conn
                .execute(
                    "INSERT INTO improvement_intake
                     (project, dedup_key, created_at_ns, outcome, candidate_id)
                     VALUES ('p', ?1, ?2, 'stored', ?3)",
                    params![
                        candidate(id).dedup_key,
                        system_time_to_ns(at(seconds)).unwrap(),
                        id
                    ],
                )
                .unwrap();
        }
        record_test(&db, "trigger-cleanup", now, policy());
        assert_eq!(intake_ids(&db), ["boundary", "recent", "trigger-cleanup"]);
        record_test(
            &db,
            "ancient",
            now,
            ImprovementWritePolicy {
                duplicate_cooldown: Duration::from_secs(365 * 86_400),
                ..policy()
            },
        );
        assert_eq!(
            intake_ids(&db),
            ["boundary", "recent", "trigger-cleanup", "ancient"]
        );
    }

    #[test]
    fn failed_intake_housekeeping_does_not_fail_the_record() {
        let db = Database::open_in_memory().unwrap();
        record_test(&db, "ancient", 100, policy());
        db.conn
            .execute_batch(
                "CREATE TRIGGER refuse_cleanup BEFORE DELETE ON improvement_intake
                 BEGIN SELECT RAISE(ROLLBACK, 'test refusal'); END;",
            )
            .unwrap();
        record_test(&db, "current", 401 * 86_400, policy());
        assert_eq!(ids(&db, "p"), ["current", "ancient"]);
        assert_eq!(intake_ids(&db), ["ancient", "current"]);
    }

    #[test]
    fn failed_intake_insertion_rolls_back_candidate() {
        let db = Database::open_in_memory().unwrap();
        db.conn
            .execute_batch(
                "CREATE TRIGGER refuse_intake BEFORE INSERT ON improvement_intake
                 BEGIN SELECT RAISE(ABORT, 'test refusal'); END;",
            )
            .unwrap();
        assert!(matches!(
            record_at(&db.conn, "p", &candidate("first"), policy(), at(100)),
            Err(StorageError::Sqlite(_))
        ));
        assert!(ids(&db, "p").is_empty());
        assert!(intake_ids(&db).is_empty());
    }

    #[test]
    fn eviction_prefers_oldest_resolved_then_oldest_overall() {
        let db = Database::open_in_memory().unwrap();
        let small = ImprovementWritePolicy {
            max_candidates: 2,
            ..policy()
        };
        record_test(&db, "first", 100, small);
        record_test(&db, "second", 101, small);
        assert!(set_status(&db.conn, "first", ImprovementStatus::Reviewed).unwrap());
        assert!(set_status(&db.conn, "second", ImprovementStatus::Dismissed).unwrap());
        record_test(&db, "third", 102, small);
        assert_eq!(ids(&db, "p"), ["third", "second"]);
        // A resolved entry takes priority even over an older unresolved entry.
        set_status(&db.conn, "second", ImprovementStatus::New).unwrap();
        set_status(&db.conn, "third", ImprovementStatus::Reviewed).unwrap();
        record_test(&db, "fourth", 103, small);
        assert_eq!(ids(&db, "p"), ["fourth", "second"]);
        record_test(&db, "fifth", 104, small);
        assert_eq!(ids(&db, "p"), ["fifth", "fourth"]);
    }

    #[test]
    fn all_new_eviction_breaks_timestamp_ties_by_insertion_order() {
        let db = Database::open_in_memory().unwrap();
        let small = ImprovementWritePolicy {
            max_candidates: 2,
            ..policy()
        };
        for id in ["z-first", "a-second", "third"] {
            record_test(&db, id, 100, small);
        }
        assert_eq!(ids(&db, "p"), ["third", "a-second"]);
    }

    #[test]
    fn shrinking_retention_evicts_multiple_rows_without_affecting_other_projects() {
        let db = Database::open_in_memory().unwrap();
        for (seconds, id) in [(100, "first"), (101, "second"), (102, "third")] {
            record_test(&db, id, seconds, policy());
        }
        record_at(&db.conn, "other", &candidate("other"), policy(), at(100)).unwrap();
        set_status(&db.conn, "second", ImprovementStatus::Reviewed).unwrap();
        record_test(
            &db,
            "fourth",
            103,
            ImprovementWritePolicy {
                max_candidates: 1,
                ..policy()
            },
        );
        assert_eq!(ids(&db, "p"), ["fourth"]);
        record_test(
            &db,
            "fifth",
            104,
            ImprovementWritePolicy {
                max_candidates: 0,
                ..policy()
            },
        );
        assert!(ids(&db, "p").is_empty());
        assert_eq!(ids(&db, "other"), ["other"]);
    }

    #[test]
    fn failed_eviction_rolls_back_insertion() {
        let db = Database::open_in_memory().unwrap();
        record_test(&db, "first", 100, policy());
        db.conn.execute_batch("CREATE TRIGGER refuse_eviction BEFORE DELETE ON improvement_candidates BEGIN SELECT RAISE(ABORT, 'test refusal'); END;").unwrap();
        let result = record_at(
            &db.conn,
            "p",
            &candidate("second"),
            ImprovementWritePolicy {
                max_candidates: 1,
                ..policy()
            },
            at(101),
        );
        assert!(matches!(result, Err(StorageError::Sqlite(_))));
        assert_eq!(ids(&db, "p"), ["first"]);
        assert_eq!(intake_ids(&db), ["first"]);
    }

    #[test]
    fn empty_fields_and_oversized_evidence_are_rejected_before_policy_checks() {
        let db = Database::open_in_memory().unwrap();
        for field in ["id", "code", "title", "evidence", "dedup_key"] {
            let mut input = candidate("invalid");
            *text_field(&mut input, field) = " \t\n".into();
            assert!(matches!(
                record(&db.conn, "p", &input, policy()),
                Err(StorageError::Serialization(_))
            ));
        }
        assert!(matches!(
            record(&db.conn, " ", &candidate("invalid"), policy()),
            Err(StorageError::Serialization(_))
        ));
        let mut input = candidate("large");
        input.evidence = "é".repeat(32_768);
        assert_eq!(
            record(&db.conn, "p", &input, policy()).unwrap(),
            stored("large")
        );
        input.id = "too-large".into();
        input.evidence.push('x');
        assert_eq!(
            record(&db.conn, "p", &input, policy()).unwrap_err(),
            StorageError::Serialization("evidence exceeds 64 KiB storage cap".into())
        );
        assert_eq!(ids(&db, "p"), ["large"]);
    }

    fn text_field<'a>(input: &'a mut NewImprovementCandidate, field: &str) -> &'a mut String {
        match field {
            "id" => &mut input.id,
            "code" => &mut input.code,
            "title" => &mut input.title,
            "evidence" => &mut input.evidence,
            "dedup_key" => &mut input.dedup_key,
            "run_id" => input.run_id.as_mut().unwrap(),
            _ => panic!("unknown test field"),
        }
    }

    #[test]
    fn reads_filter_order_and_bound_results() {
        let db = Database::open_in_memory().unwrap();
        let large = ImprovementWritePolicy {
            daily_limit: 2000,
            max_candidates: 2000,
            ..policy()
        };
        for index in 0..1002 {
            record_test(&db, &format!("candidate-{index}"), index, large);
        }
        set_status(&db.conn, "candidate-2", ImprovementStatus::Reviewed).unwrap();
        let rows = db.improvement_candidates("p", None, usize::MAX).unwrap();
        assert_eq!(rows.len(), 1000);
        assert_eq!(rows[0].id, "candidate-1001");
        assert_eq!(rows[999].id, "candidate-2");
        assert_eq!(db.improvement_candidates("p", None, 1).unwrap().len(), 1);
        assert!(db.improvement_candidates("p", None, 0).unwrap().is_empty());
        assert!(
            db.improvement_candidates("other", None, 10)
                .unwrap()
                .is_empty()
        );
        let reviewed = db
            .improvement_candidates("p", Some(ImprovementStatus::Reviewed), 10)
            .unwrap();
        assert_eq!(reviewed.len(), 1);
        assert_eq!(reviewed[0].id, "candidate-2");
    }

    #[test]
    fn enums_round_trip_and_reject_unknown_values() {
        for source in [ImprovementSource::Diagnostic, ImprovementSource::Lesson] {
            assert_eq!(ImprovementSource::parse(source.as_str()).unwrap(), source);
            assert_eq!(
                serde_json::to_string(&source).unwrap(),
                format!("\"{}\"", source.as_str())
            );
        }
        for severity in [
            ImprovementSeverity::Info,
            ImprovementSeverity::Warning,
            ImprovementSeverity::Error,
        ] {
            assert_eq!(
                ImprovementSeverity::parse(severity.as_str()).unwrap(),
                severity
            );
            assert_eq!(
                serde_json::to_string(&severity).unwrap(),
                format!("\"{}\"", severity.as_str())
            );
        }
        for status in [
            ImprovementStatus::New,
            ImprovementStatus::Reviewed,
            ImprovementStatus::Dismissed,
        ] {
            assert_eq!(ImprovementStatus::parse(status.as_str()).unwrap(), status);
            assert_eq!(status.is_resolved(), status != ImprovementStatus::New);
            assert_eq!(
                serde_json::to_string(&status).unwrap(),
                format!("\"{}\"", status.as_str())
            );
        }
        assert!(matches!(
            ImprovementSource::parse("unknown"),
            Err(StorageError::Serialization(_))
        ));
        assert!(matches!(
            ImprovementSeverity::parse("unknown"),
            Err(StorageError::Serialization(_))
        ));
        assert!(matches!(
            ImprovementStatus::parse("unknown"),
            Err(StorageError::Serialization(_))
        ));
    }

    #[test]
    fn malformed_persisted_values_are_reported() {
        let db = Database::open_in_memory().unwrap();
        record_test(&db, "first", 100, policy());
        for (column, original) in [
            ("source", "diagnostic"),
            ("severity", "warning"),
            ("status", "new"),
        ] {
            let sql = format!("UPDATE improvement_candidates SET {column} = ?1");
            db.conn.execute(&sql, ["unknown"]).unwrap();
            assert!(matches!(
                db.improvement_candidate("first"),
                Err(StorageError::Serialization(_))
            ));
            assert!(matches!(
                db.improvement_candidates("p", None, 10),
                Err(StorageError::Serialization(_))
            ));
            db.conn.execute(&sql, [original]).unwrap();
        }
        db.conn
            .execute("UPDATE improvement_candidates SET created_at_ns = -1", [])
            .unwrap();
        assert!(matches!(
            db.improvement_candidate("first"),
            Err(StorageError::OutOfRange(_))
        ));
    }

    #[test]
    fn writer_retention_cannot_bypass_cooldown_or_daily_limit() {
        let dir = tempfile::tempdir().unwrap();
        let config = file_config(&dir);
        let writer = Storage::open(config.clone()).unwrap();
        let handle = writer.handle();
        let limited = ImprovementWritePolicy {
            duplicate_cooldown: Duration::from_secs(60),
            daily_limit: 2,
            max_candidates: 1,
        };
        let outcomes: Vec<_> = ["A", "B", "A", "C", "D"]
            .into_iter()
            .map(|id| {
                handle
                    .record_improvement_candidate("p", candidate(id), limited)
                    .unwrap()
            })
            .collect();
        assert_eq!(
            outcomes,
            [
                stored("A"),
                stored("B"),
                ImprovementRecordOutcome::Duplicate {
                    existing_id: "A".into()
                },
                ImprovementRecordOutcome::RateLimited,
                ImprovementRecordOutcome::RateLimited,
            ]
        );
        let db = Database::open(&config).unwrap();
        assert_eq!(ids(&db, "p"), ["B"]);
        assert_eq!(intake_ids(&db), ["A", "B"]);
        // B is still retained after the probe. Allow one more stored candidate
        // solely to evict B, then retry B under the original policy.
        assert_eq!(
            handle
                .record_improvement_candidate(
                    "p",
                    candidate("C"),
                    ImprovementWritePolicy {
                        daily_limit: 3,
                        ..limited
                    },
                )
                .unwrap(),
            stored("C")
        );
        assert!(db.improvement_candidate("B").unwrap().is_none());
        assert_eq!(
            handle
                .record_improvement_candidate("p", candidate("B"), limited)
                .unwrap(),
            ImprovementRecordOutcome::Duplicate {
                existing_id: "B".into()
            }
        );
    }

    #[test]
    fn writer_records_deduplicates_and_rate_limits() {
        let dir = tempfile::tempdir().unwrap();
        let config = file_config(&dir);
        let writer = Storage::open(config.clone()).unwrap();
        let handle = writer.handle();
        let limited = ImprovementWritePolicy {
            daily_limit: 1,
            ..policy()
        };
        assert_eq!(
            handle
                .record_improvement_candidate("p", candidate("first"), limited)
                .unwrap(),
            stored("first")
        );
        assert_eq!(
            handle
                .record_improvement_candidate("p", candidate("first"), limited)
                .unwrap(),
            ImprovementRecordOutcome::Duplicate {
                existing_id: "first".into()
            }
        );
        assert_eq!(
            handle
                .record_improvement_candidate("p", candidate("second"), limited)
                .unwrap(),
            ImprovementRecordOutcome::RateLimited
        );
        assert_eq!(ids(&Database::open(&config).unwrap(), "p"), ["first"]);
    }

    #[test]
    fn writer_status_transitions_and_draft_attachment() {
        let dir = tempfile::tempdir().unwrap();
        let config = file_config(&dir);
        let writer = Storage::open(config.clone()).unwrap();
        let handle = writer.handle();
        handle
            .record_improvement_candidate("p", candidate("first"), policy())
            .unwrap();
        let db = Database::open(&config).unwrap();
        for status in [
            ImprovementStatus::Reviewed,
            ImprovementStatus::Dismissed,
            ImprovementStatus::New,
        ] {
            assert!(handle.set_improvement_status("first", status).unwrap());
            assert_eq!(
                db.improvement_candidate("first").unwrap().unwrap().status,
                status
            );
            assert!(!handle.set_improvement_status("missing", status).unwrap());
        }
        for path in ["first.md", "drafts/first.md"] {
            assert!(handle.attach_improvement_draft("first", path).unwrap());
            assert_eq!(
                db.improvement_candidate("first")
                    .unwrap()
                    .unwrap()
                    .draft_path
                    .as_deref(),
                Some(path)
            );
        }
        assert!(
            !handle
                .attach_improvement_draft("missing", "missing.md")
                .unwrap()
        );
        assert!(matches!(
            handle.attach_improvement_draft("first", " \n"),
            Err(StorageError::Serialization(_))
        ));
        assert_eq!(
            db.improvement_candidate("first")
                .unwrap()
                .unwrap()
                .draft_path
                .as_deref(),
            Some("drafts/first.md")
        );
    }

    #[test]
    fn writer_drop_and_reopen_preserve_candidate_status_and_draft() {
        let dir = tempfile::tempdir().unwrap();
        let config = file_config(&dir);
        let expected;
        {
            let writer = Storage::open(config.clone()).unwrap();
            let handle = writer.handle();
            let mut input = candidate("durable");
            input.source = ImprovementSource::Lesson;
            input.severity = ImprovementSeverity::Info;
            input.run_id = None;
            handle
                .record_improvement_candidate("p", input, policy())
                .unwrap();
            handle
                .set_improvement_status("durable", ImprovementStatus::Reviewed)
                .unwrap();
            handle
                .attach_improvement_draft("durable", "durable.md")
                .unwrap();
            expected = Database::open(&config)
                .unwrap()
                .improvement_candidate("durable")
                .unwrap()
                .unwrap();
        }
        let reopened = Database::open(&config).unwrap();
        assert_eq!(
            reopened.improvement_candidate("durable").unwrap(),
            Some(expected)
        );
        let writer = Storage::open(config).unwrap();
        assert_eq!(
            writer
                .handle()
                .record_improvement_candidate("p", candidate("durable"), policy())
                .unwrap(),
            ImprovementRecordOutcome::Duplicate {
                existing_id: "durable".into()
            }
        );
    }

    #[test]
    fn writer_shutdown_and_storage_limits_apply_to_new_commands() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = file_config(&dir);
        config.hard_limits.max_db_bytes = 0;
        let writer = Storage::open(config.clone()).unwrap();
        let handle = writer.handle();
        assert!(matches!(
            handle.record_improvement_candidate("p", candidate("first"), policy()),
            Err(StorageError::Serialization(_))
        ));
        assert!(matches!(
            handle.set_improvement_status("first", ImprovementStatus::Reviewed),
            Err(StorageError::Serialization(_))
        ));
        assert!(matches!(
            handle.attach_improvement_draft("first", "first.md"),
            Err(StorageError::Serialization(_))
        ));
        assert!(ids(&Database::open(&config).unwrap(), "p").is_empty());
        drop(writer);
        assert_eq!(
            handle
                .record_improvement_candidate("p", candidate("first"), policy())
                .unwrap_err(),
            StorageError::WriterClosed
        );
        assert_eq!(
            handle
                .set_improvement_status("first", ImprovementStatus::Reviewed)
                .unwrap_err(),
            StorageError::WriterClosed
        );
        assert_eq!(
            handle
                .attach_improvement_draft("first", "first.md")
                .unwrap_err(),
            StorageError::WriterClosed
        );
    }

    #[test]
    fn concurrent_storage_instances_cannot_bypass_dedup() {
        let dir = tempfile::tempdir().unwrap();
        let config = file_config(&dir);
        let writers = [
            Storage::open(config.clone()).unwrap(),
            Storage::open(config.clone()).unwrap(),
        ];
        let barrier = std::sync::Barrier::new(8);
        let outcomes = std::thread::scope(|scope| {
            let jobs: Vec<_> = (0..8)
                .map(|index| {
                    let handle = writers[index % 2].handle();
                    let barrier = &barrier;
                    scope.spawn(move || {
                        let mut input = candidate(&format!("concurrent-{index}"));
                        input.dedup_key = "shared-key".into();
                        barrier.wait();
                        handle
                            .record_improvement_candidate("p", input, policy())
                            .unwrap()
                    })
                })
                .collect();
            jobs.into_iter()
                .map(|job| job.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, ImprovementRecordOutcome::Stored { .. }))
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, ImprovementRecordOutcome::Duplicate { .. }))
                .count(),
            7
        );
        assert_eq!(ids(&Database::open(&config).unwrap(), "p").len(), 1);
    }

    #[test]
    fn v9_database_upgrades_without_losing_existing_data() {
        let dir = tempfile::tempdir().unwrap();
        let config = file_config(&dir);
        {
            let conn = Connection::open(&config.db_path).unwrap();
            for migration in &crate::migrations::MIGRATIONS[..9] {
                conn.execute_batch(migration).unwrap();
            }
            conn.pragma_update(None, "user_version", 9).unwrap();
            conn.execute(
                "INSERT INTO run_ledger VALUES (1, 'run-1', 'preserved', 10)",
                [],
            )
            .unwrap();
        }
        let db = Database::open(&config).unwrap();
        assert_eq!(db.pragma_i64("user_version").unwrap(), 15);
        assert_eq!(db.run_ledger_all().unwrap()[0].body, "preserved");
        assert!(ids(&db, "p").is_empty());
        let writer = Storage::open(config).unwrap();
        assert_eq!(
            writer
                .handle()
                .record_improvement_candidate("p", candidate("first"), policy())
                .unwrap(),
            stored("first")
        );
    }

    #[test]
    fn secrets_in_all_persisted_text_fields_are_rejected() {
        const SENTINEL: &str = "evorch-improvement-known-sentinel-0123456789";
        const CHILD_ENV: &str = "EVORCH_IMPROVEMENT_SECRET_TEST_CHILD";
        if std::env::var_os(CHILD_ENV).is_none() {
            // Set the credential only in a fresh process, never in the parallel test runner.
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "improvement::tests::secrets_in_all_persisted_text_fields_are_rejected",
                    "--nocapture",
                ])
                .env(CHILD_ENV, "1")
                .env("GH_TOKEN", SENTINEL)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "child test failed: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let config = file_config(&dir);
        let writer = Storage::open(config.clone()).unwrap();
        let handle = writer.handle();
        for field in [
            "id",
            "code",
            "title",
            "evidence",
            "dedup_key",
            "run_id",
            "project",
        ] {
            let mut input = candidate("rejected");
            let project = if field == "project" {
                SENTINEL
            } else {
                *text_field(&mut input, field) = format!("prefix-{SENTINEL}-suffix");
                "p"
            };
            let error = handle
                .record_improvement_candidate(project, input, policy())
                .unwrap_err();
            assert!(
                matches!(error, StorageError::SecretDetected { entity: "improvement", field: found, .. } if found == field)
            );
            assert!(error.to_string().contains("known-credential-value"));
            assert!(!error.to_string().contains(SENTINEL));
        }
        let db = Database::open(&config).unwrap();
        let count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM improvement_candidates", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0);
        handle
            .record_improvement_candidate("p", candidate("safe"), policy())
            .unwrap();
        let error = handle
            .attach_improvement_draft("safe", &format!("{SENTINEL}.md"))
            .unwrap_err();
        assert!(matches!(
            error,
            StorageError::SecretDetected {
                field: "draft_path",
                ..
            }
        ));
        assert!(!error.to_string().contains(SENTINEL));
        assert_eq!(
            db.improvement_candidate("safe")
                .unwrap()
                .unwrap()
                .draft_path,
            None
        );
    }
}
