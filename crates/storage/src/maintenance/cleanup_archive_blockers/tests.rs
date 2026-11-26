//! The cleanup against synthetic databases in temporary directories only.

use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, params};

use super::apply::{Fault, apply_with};
use super::*;
use crate::maintenance::offline::lease_file;
use crate::{Database, StorageConfig};

struct F {
    dir: tempfile::TempDir,
    config: PathBuf,
    db: PathBuf,
    sidebar: PathBuf,
}

impl F {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config");
        fs::create_dir(&config).unwrap();
        let db = config.join("evorch-events.db");
        let database = Database::open(&StorageConfig {
            db_path: db.clone(),
            ..Default::default()
        })
        .unwrap();
        drop(database);
        lease_file(&db).unwrap();
        let f = Self {
            sidebar: config.join("sidebar.json"),
            dir,
            config,
            db,
        };
        f.sidebar(&[]);
        f
    }

    fn conn(&self) -> Connection {
        let conn = Connection::open(&self.db).unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        conn
    }

    fn sql(&self, text: &str, p: impl rusqlite::Params) {
        self.conn().execute(text, p).unwrap();
    }

    fn sidebar(&self, threads: &[(&str, &[&str])]) {
        let items: Vec<_> = threads
            .iter()
            .map(|(id, runs)| serde_json::json!({"id": id, "run_ids": runs, "root_run_id": null}))
            .collect();
        fs::write(
            &self.sidebar,
            serde_json::json!({"threads": items}).to_string(),
        )
        .unwrap();
    }

    fn question(&self, id: &str, run: &str, root: &str) {
        let question = event_bus::UserQuestion {
            id: id.into(),
            run_id: run.into(),
            root_run_id: root.into(),
            root_name: "chat".into(),
            recipient_run_ids: vec![],
            title: format!("title of {id}"),
            options: vec![],
            blocking: true,
            answer: None,
            answer_origin: None,
            cancelled: false,
            responder: None,
            routing_revision: 0,
            management_revision: None,
        };
        self.sql(
            "INSERT INTO user_questions(id,run_id,root_run_id,root_name,payload) VALUES(?1,?2,?3,'chat',?4)",
            params![id, run, root, serde_json::to_string(&question).unwrap()],
        );
    }

    fn lane(&self, id: &str, thread: &str, lead: &str, policy: &str) {
        let brief = serde_json::json!({"objective":"o","inputs":[],"deliverables":[],"acceptance_criteria":[],"constraints":[],"report_policy":policy}).to_string();
        let lead = (policy != "detached").then_some(lead);
        self.sql(
            "INSERT INTO lanes(lane_id,thread_id,origin_thread_id,origin_run_id,creation_key,request_hash,scope,lead_thread_id,policy,management_revision,management_projection_revision,brief_json,brief_revision,start_seed,root_run_id,generation,created_at_ns,updated_at_ns,last_activity_at_ns) \
             VALUES(?1,?2,'origin','r0',?1,'h','lane',?3,?4,1,1,?5,1,'{}','root-'||?1,1,0,0,0)",
            params![id, thread, lead, policy, brief],
        );
    }

    fn close_lane_row(&self, lane: &str) {
        self.sql(
            "INSERT INTO lane_closes(close_id,lane_id,kind,payload) VALUES('close-'||?1,?1,'human_closed','{}')",
            [lane],
        );
        self.sql(
            "UPDATE lanes SET current_close_id='close-'||?1 WHERE lane_id=?1",
            [lane],
        );
    }

    fn question_notification(
        &self,
        id: &str,
        lane: &str,
        question: &str,
        source: &str,
        dest: &str,
    ) {
        self.sql(
            "INSERT INTO lane_notifications(notification_id,lane_id,management_revision,event_kind,semantic_key,source_thread_id,destination_thread_id,payload,state) \
             VALUES(?1,?2,1,'question','question:'||?3,?4,?5,?6,'pending')",
            params![
                id,
                lane,
                question,
                source,
                dest,
                serde_json::json!({"kind": {"kind": "question", "question_id": question, "routing_revision": 1}}).to_string()
            ],
        );
    }

    fn tombstone_run(&self, run: &str) {
        self.sql(
            "INSERT INTO run_tombstones(run_id,thread_id,family_key) VALUES(?1,'gone','gone')",
            [run],
        );
    }

    fn tombstone_thread(&self, thread: &str) {
        self.sql(
            "INSERT INTO thread_tombstones(thread_id,family_key,deleted_at_ns) VALUES(?1,?1,1)",
            [thread],
        );
    }

    fn preview(&self) -> CleanupPlan {
        preview_archive_blocker_cleanup(&CleanupPreviewOptions {
            db: self.db.clone(),
            config_dir: self.config.clone(),
            sidebar: self.sidebar.clone(),
        })
        .unwrap()
    }

    fn save(&self, plan: &CleanupPlan) -> PathBuf {
        let path = self.dir.path().join("plan.json");
        fs::write(&path, serde_json::to_vec_pretty(plan).unwrap()).unwrap();
        path
    }

    fn options(&self, plan: &Path, backup: &Path) -> CleanupApplyOptions {
        CleanupApplyOptions {
            db: self.db.clone(),
            config_dir: self.config.clone(),
            sidebar: self.sidebar.clone(),
            plan: plan.to_owned(),
            backup: backup.to_owned(),
        }
    }

    fn backup_path(&self) -> PathBuf {
        self.dir.path().join("backup.db")
    }

    /// Everything the cleanup may touch, as one comparable value.
    fn state(&self) -> Vec<String> {
        let conn = self.conn();
        let mut out = Vec::new();
        for query in [
            "SELECT id||'|'||answered||'|'||cancelled||'|'||payload FROM user_questions ORDER BY id",
            "SELECT notification_id||'|'||event_kind||'|'||state FROM lane_notifications ORDER BY notification_id",
            "SELECT lane_id||'|'||COALESCE(current_close_id,'-') FROM lanes ORDER BY lane_id",
            "SELECT close_id||'|'||kind FROM lane_closes ORDER BY close_id",
            "SELECT plan_hash FROM archive_blocker_cleanup_receipts",
        ] {
            let mut statement = conn.prepare(query).unwrap();
            out.extend(
                statement
                    .query_map([], |row| row.get::<_, String>(0))
                    .unwrap()
                    .map(Result::unwrap),
            );
        }
        out
    }

    fn files(&self) -> Vec<(String, u64)> {
        let mut out: Vec<_> = fs::read_dir(&self.config)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (
                    entry.file_name().to_string_lossy().into_owned(),
                    fs::metadata(entry.path()).unwrap().len(),
                )
            })
            .collect();
        out.sort();
        out
    }

    fn standard() -> Self {
        let f = Self::new();
        f.question("q1", "run-a", "run-a");
        f.question("q2", "run-b", "run-b");
        f.lane("lane-open", "lane-thread", "lead-thread", "attached");
        f.lane("lane-summary", "summary-thread", "lead-thread", "summary");
        f.lane("lane-detached", "detached-thread", "x", "detached");
        f.lane("lane-closed", "closed-thread", "lead-thread", "attached");
        f.close_lane_row("lane-closed");
        f.question_notification("n1", "lane-open", "q1", "lane-thread", "lead-thread");
        f
    }
}

fn error_text(result: Result<CleanupApplySummary, StorageError>) -> String {
    result.unwrap_err().to_string()
}

#[test]
fn preview_lists_every_target_and_reason_and_changes_nothing() {
    let f = F::standard();
    let db_bytes = fs::read(&f.db).unwrap();
    let files = f.files();
    let sidebar_bytes = fs::read(&f.sidebar).unwrap();
    let modified = fs::metadata(&f.db).unwrap().modified().unwrap();

    let plan = f.preview();

    assert_eq!(plan.plan_version, PLAN_VERSION);
    assert!(plan.apply_allowed);
    assert_eq!(
        plan.manifest
            .questions
            .iter()
            .map(|q| q.id.as_str())
            .collect::<Vec<_>>(),
        ["q1", "q2"]
    );
    assert_eq!(plan.manifest.questions[0].pending_notification_ids, ["n1"]);
    assert_eq!(
        plan.manifest
            .lanes
            .iter()
            .map(|l| l.lane_id.as_str())
            .collect::<Vec<_>>(),
        ["lane-open", "lane-summary"]
    );
    assert_eq!(plan.counts.question_notifications_to_obsolete, 1);
    assert_eq!(
        plan.counts.closed_notification_recipients, 1,
        "one lead, counted once"
    );
    let reasons: Vec<_> = plan
        .manifest
        .exclusions
        .iter()
        .map(|e| (e.id.as_str(), e.reason.as_str()))
        .collect();
    assert!(
        reasons
            .iter()
            .any(|(id, r)| *id == "lane-detached" && r.contains("detached"))
    );
    assert!(
        reasons
            .iter()
            .any(|(id, r)| *id == "lane-closed" && r.contains("already closed"))
    );
    assert!(plan.notices.iter().any(|n| n.contains("7 days")));
    assert_ne!(
        plan.target_manifest_sha256, plan.source_image_sha256,
        "the manifest hash does not depend on the database image"
    );

    assert_eq!(fs::read(&f.db).unwrap(), db_bytes);
    assert_eq!(
        f.files(),
        files,
        "no file or sidecar was created or removed"
    );
    assert_eq!(fs::read(&f.sidebar).unwrap(), sidebar_bytes);
    assert_eq!(fs::metadata(&f.db).unwrap().modified().unwrap(), modified);
}

#[test]
fn the_manifest_hash_follows_the_targets_not_the_image() {
    let f = F::standard();
    let first = f.preview();
    f.sql("INSERT INTO sessions (id, status, created_at_ns, updated_at_ns) VALUES ('s','running',0,0)", []);
    let second = f.preview();
    assert_eq!(first.target_manifest_sha256, second.target_manifest_sha256);
    assert_ne!(first.source_image_sha256, second.source_image_sha256);
    assert_ne!(first.plan_hash, second.plan_hash);
    f.question("q3", "run-c", "run-c");
    assert_ne!(
        f.preview().target_manifest_sha256,
        first.target_manifest_sha256
    );
}

#[test]
fn apply_cancels_obsoletes_discards_once_and_a_rerun_changes_nothing() {
    let f = F::standard();
    let plan = f.preview();
    let plan_path = f.save(&plan);
    let backup = f.backup_path();
    let before_ack = f.state();

    let summary = apply_archive_blocker_cleanup(&f.options(&plan_path, &backup)).unwrap();

    assert_eq!(summary.status, "applied");
    assert!(summary.checkpoint_truncated);
    assert_eq!(
        (
            summary.cancelled_questions,
            summary.obsoleted_notifications,
            summary.closed_lanes
        ),
        (2, 1, 2)
    );
    let conn = f.conn();
    for id in ["q1", "q2"] {
        let (answered, cancelled, payload): (i64, i64, String) = conn
            .query_row(
                "SELECT answered,cancelled,payload FROM user_questions WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((answered, cancelled), (0, 1));
        let stored: event_bus::UserQuestion = serde_json::from_str(&payload).unwrap();
        assert_eq!(stored.status(), event_bus::QuestionStatus::Cancelled);
        assert!(stored.answer.is_none() && stored.answer_origin.is_none());
    }
    let state: String = conn
        .query_row(
            "SELECT state FROM lane_notifications WHERE notification_id='n1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state, "obsolete");
    let answers: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM lane_notifications WHERE event_kind IN ('answer','ack')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(answers, 0, "no answer or acknowledgement is produced");
    for lane in ["lane-open", "lane-summary"] {
        let kind: String = conn
            .query_row(
                "SELECT c.kind FROM lanes l JOIN lane_closes c ON c.close_id=l.current_close_id WHERE l.lane_id=?1",
                [lane],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kind, "discarded");
    }
    let closed_notices: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM lane_notifications WHERE event_kind='closed' AND state='pending'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(closed_notices, 2);
    let untouched: String = conn
        .query_row(
            "SELECT COALESCE(current_close_id,'-') FROM lanes WHERE lane_id='lane-detached'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(untouched, "-");
    drop(conn);
    assert!(
        !crate::db::suffixed_path(&f.db, "-wal").exists()
            || fs::metadata(crate::db::suffixed_path(&f.db, "-wal"))
                .unwrap()
                .len()
                == 0
    );
    assert_eq!(
        sha256_file(&backup).unwrap(),
        plan.source_image_sha256,
        "backup is the previewed image"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&backup).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    assert_ne!(f.state(), before_ack);

    let after = f.state();
    let again = apply_archive_blocker_cleanup(&f.options(&plan_path, &backup)).unwrap();
    assert_eq!(again.status, "already_applied");
    assert_eq!(
        f.state(),
        after,
        "no question, lane, notification or receipt changed"
    );
    assert_eq!(
        after.iter().filter(|row| row.len() == 64).count(),
        1,
        "one receipt"
    );
}

#[test]
fn a_cancelled_question_cannot_be_answered_or_come_back() {
    let f = F::standard();
    let plan = f.preview();
    apply_archive_blocker_cleanup(&f.options(&f.save(&plan), &f.backup_path())).unwrap();
    let conn = f.conn();
    let again = crate::repo::user_questions::answer(
        &conn,
        "q1",
        "yes",
        &event_bus::AnswerOrigin::Host {
            channel: Default::default(),
        },
        crate::AnswerMode::Idempotent,
    );
    assert!(
        matches!(again, Err(StorageError::QuestionCancelled)),
        "{again:?}"
    );
    assert!(
        conn.execute("UPDATE user_questions SET cancelled=0 WHERE id='q1'", [])
            .is_err()
    );
    assert!(
        conn.execute("UPDATE user_questions SET answered=1 WHERE id='q1'", [])
            .is_err()
    );
    assert!(
        crate::repo::user_questions::pending(&conn)
            .unwrap()
            .is_empty()
    );
    let history = crate::repo::user_questions::for_run(&conn, "run-a").unwrap();
    assert_eq!(history.len(), 1);
    assert!(history[0].cancelled && history[0].answer.is_none());
    assert!(history[0].answer_message().is_none());
}

#[test]
fn a_tampered_or_foreign_plan_is_refused_without_a_change() {
    let f = F::standard();
    let plan = f.preview();
    let before = f.state();
    let backup = f.backup_path();
    let mut value = serde_json::to_value(&plan).unwrap();
    value["manifest"]["questions"][0]["title"] = "something else".into();
    let edited = f.dir.path().join("edited.json");
    fs::write(&edited, value.to_string()).unwrap();
    assert!(
        error_text(apply_archive_blocker_cleanup(&f.options(&edited, &backup))).contains("edited")
    );

    let mut value = serde_json::to_value(&plan).unwrap();
    value["canonical_db_path"] = "/elsewhere/other.db".into();
    fs::write(&edited, value.to_string()).unwrap();
    assert!(
        error_text(apply_archive_blocker_cleanup(&f.options(&edited, &backup)))
            .contains("another database")
    );

    let mut value = serde_json::to_value(&plan).unwrap();
    value["plan_version"] = 99.into();
    fs::write(&edited, value.to_string()).unwrap();
    assert!(apply_archive_blocker_cleanup(&f.options(&edited, &backup)).is_err());

    let mut value = serde_json::to_value(&plan).unwrap();
    value["plan_hash"] = "0".repeat(64).into();
    fs::write(&edited, value.to_string()).unwrap();
    assert!(
        error_text(apply_archive_blocker_cleanup(&f.options(&edited, &backup)))
            .contains("plan hash")
    );

    assert_eq!(f.state(), before);
    assert!(!backup.exists());
}

#[test]
fn a_plan_from_before_a_database_change_is_refused() {
    let f = F::standard();
    let plan = f.preview();
    let path = f.save(&plan);
    let before = f.state();
    // A question that appeared after the preview: the manifest differs.
    f.question("q-new", "run-new", "run-new");
    let message = error_text(apply_archive_blocker_cleanup(
        &f.options(&path, &f.backup_path()),
    ));
    assert!(message.contains("preview again"), "{message}");
    assert!(!f.backup_path().exists());
    assert_eq!(
        f.state().len(),
        before.len() + 1,
        "only the new question appeared"
    );
}

#[test]
fn an_unrelated_database_change_with_the_same_manifest_still_refuses() {
    let f = F::standard();
    let plan = f.preview();
    let path = f.save(&plan);
    f.sql("INSERT INTO sessions (id, status, created_at_ns, updated_at_ns) VALUES ('s','running',0,0)", []);
    let message = error_text(apply_archive_blocker_cleanup(
        &f.options(&path, &f.backup_path()),
    ));
    assert!(message.contains("not the image"), "{message}");
    assert!(!f.backup_path().exists());
}

#[test]
fn a_non_empty_wal_is_refused_by_preview_and_apply_and_never_recovered() {
    let f = F::standard();
    let plan = f.preview();
    let path = f.save(&plan);
    let live = f.conn();
    live.execute(
        "INSERT INTO sessions (id, status, created_at_ns, updated_at_ns) VALUES ('s','running',0,0)",
        [],
    )
    .unwrap();
    let wal = crate::db::suffixed_path(&f.db, "-wal");
    let wal_bytes = fs::metadata(&wal).unwrap().len();
    assert!(wal_bytes > 0);

    let preview = preview_archive_blocker_cleanup(&CleanupPreviewOptions {
        db: f.db.clone(),
        config_dir: f.config.clone(),
        sidebar: f.sidebar.clone(),
    });
    assert!(preview.unwrap_err().to_string().contains("WAL"));
    let message = error_text(apply_archive_blocker_cleanup(
        &f.options(&path, &f.backup_path()),
    ));
    assert!(message.contains("WAL"), "{message}");
    assert_eq!(
        fs::metadata(&wal).unwrap().len(),
        wal_bytes,
        "the WAL was not touched"
    );
    assert!(!f.backup_path().exists());
    drop(live);
}

#[test]
fn a_held_maintenance_lease_or_exclusive_connection_refuses_apply() {
    let f = F::standard();
    let plan = f.preview();
    let path = f.save(&plan);
    let held = lease_file(&f.db).unwrap();
    held.try_lock().unwrap();
    assert!(
        error_text(apply_archive_blocker_cleanup(
            &f.options(&path, &f.backup_path())
        ))
        .contains("in use")
    );
    assert!(
        preview_archive_blocker_cleanup(&CleanupPreviewOptions {
            db: f.db.clone(),
            config_dir: f.config.clone(),
            sidebar: f.sidebar.clone(),
        })
        .is_err()
    );
    drop(held);

    let other = f.conn();
    other.execute_batch("BEGIN EXCLUSIVE").unwrap();
    assert!(apply_archive_blocker_cleanup(&f.options(&path, &f.backup_path())).is_err());
    other.execute_batch("ROLLBACK").unwrap();
    drop(other);
    assert!(!f.backup_path().exists());
}

#[test]
fn every_failure_point_rolls_back_and_a_retry_completes() {
    for fault in [
        Fault::AfterBackup,
        Fault::AfterQuestions,
        Fault::AfterNotifications,
        Fault::AfterLanes,
        Fault::BeforeCommit,
    ] {
        let f = F::standard();
        let plan = f.preview();
        let path = f.save(&plan);
        let before = f.state();
        let backup = f.backup_path();
        let error = apply_with(&f.options(&path, &backup), fault).unwrap_err();
        assert!(error.to_string().contains("injected"), "{fault:?}: {error}");
        assert_eq!(f.state(), before, "{fault:?} left a change behind");
        assert!(backup.exists(), "{fault:?}: the verified backup stays");
        let done = apply_archive_blocker_cleanup(&f.options(&path, &backup)).unwrap();
        assert_eq!(done.status, "applied", "{fault:?}");
        assert!(done.backup_reused, "{fault:?}: the same image is reused");
    }
}

#[test]
fn a_failure_after_commit_keeps_one_receipt_and_the_rerun_confirms_it() {
    let f = F::standard();
    let plan = f.preview();
    let path = f.save(&plan);
    let backup = f.backup_path();
    let error = apply_with(&f.options(&path, &backup), Fault::AfterCommit).unwrap_err();
    assert!(error.to_string().contains("injected"));
    let after = f.state();
    assert_eq!(after.iter().filter(|row| row.len() == 64).count(), 1);
    assert!(after.iter().any(|row| row.starts_with("q1|0|1|")));
    let again = apply_archive_blocker_cleanup(&f.options(&path, &backup)).unwrap();
    assert_eq!(again.status, "already_applied");
    assert_eq!(f.state(), after);
}

#[test]
fn a_checkpoint_error_after_commit_is_reported_as_committed() {
    let f = F::standard();
    let plan = f.preview();
    let path = f.save(&plan);
    let backup = f.backup_path();
    let error = apply_with(&f.options(&path, &backup), Fault::Checkpoint).unwrap_err();
    let message = error.to_string();
    assert!(message.contains("COMMITTED"), "{message}");
    assert!(message.contains("already_applied"), "{message}");
    let after = f.state();
    assert_eq!(after.iter().filter(|row| row.len() == 64).count(), 1);
    let again = apply_archive_blocker_cleanup(&f.options(&path, &backup)).unwrap();
    assert_eq!(again.status, "already_applied");
    assert_eq!(f.state(), after);
}

#[test]
fn backups_are_never_overwritten_or_aliased() {
    let f = F::standard();
    let plan = f.preview();
    let path = f.save(&plan);
    let before = f.state();

    let foreign = f.dir.path().join("foreign.db");
    fs::write(&foreign, b"not this image").unwrap();
    let message = error_text(apply_archive_blocker_cleanup(&f.options(&path, &foreign)));
    assert!(message.contains("different image"), "{message}");
    assert_eq!(fs::read(&foreign).unwrap(), b"not this image");

    let same = error_text(apply_archive_blocker_cleanup(&f.options(&path, &f.db)));
    assert!(same.contains("backup path"), "{same}");
    let wal_name = error_text(apply_archive_blocker_cleanup(
        &f.options(&path, &crate::db::suffixed_path(&f.db, "-wal")),
    ));
    assert!(wal_name.contains("backup path"), "{wal_name}");

    let link = f.dir.path().join("link.db");
    std::os::unix::fs::symlink(&f.db, &link).unwrap();
    assert!(apply_archive_blocker_cleanup(&f.options(&path, &link)).is_err());
    let hard = f.dir.path().join("hard.db");
    fs::hard_link(&f.db, &hard).unwrap();
    assert!(apply_archive_blocker_cleanup(&f.options(&path, &hard)).is_err());
    fs::remove_file(&hard).unwrap();

    let missing_parent = f.dir.path().join("no-dir").join("b.db");
    assert!(apply_archive_blocker_cleanup(&f.options(&path, &missing_parent)).is_err());
    assert_eq!(f.state(), before, "no refused backup let the data change");

    let fresh = f.dir.path().join("fresh.db");
    apply_archive_blocker_cleanup(&f.options(&path, &fresh)).unwrap();
    let leftovers: Vec<_> = fs::read_dir(f.dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
        .collect();
    assert!(leftovers.is_empty());
}

#[test]
fn a_remnant_question_is_excluded_only_when_no_live_binding_remains() {
    // Requester deleted but the root is live: the fence rejects the update and the
    // question still protects the root. Not a remnant.
    let f = F::new();
    f.question("q-half", "run-dead", "run-live");
    f.tombstone_run("run-dead");
    let plan = f.preview();
    assert!(!plan.apply_allowed);
    assert_eq!(plan.manifest.unprocessable[0].id, "q-half");
    assert!(plan.manifest.exclusions.is_empty());

    // Every run deleted and nothing binds them: excluded with a reason.
    let f = F::new();
    f.question("q-gone", "run-dead", "run-dead");
    f.tombstone_run("run-dead");
    let plan = f.preview();
    assert!(plan.apply_allowed, "{:?}", plan.manifest.unprocessable);
    assert_eq!(plan.manifest.exclusions[0].id, "q-gone");
    assert!(plan.manifest.questions.is_empty());
    assert_eq!(plan.manifest.run_bindings[0].run_id, "run-dead");
    let before = f.state();
    apply_archive_blocker_cleanup(&f.options(&f.save(&plan), &f.backup_path())).unwrap();
    let after = f.state();
    assert!(
        after.iter().any(|row| row.starts_with("q-gone|0|0|")),
        "the remnant stays pending"
    );
    assert_eq!(after.len(), before.len() + 1, "only the receipt was added");

    // The database no longer binds the run but the sidebar does.
    let f = F::new();
    f.question("q-side", "run-dead", "run-dead");
    f.tombstone_run("run-dead");
    f.sidebar(&[("live-thread", &["run-dead"])]);
    let plan = f.preview();
    assert!(!plan.apply_allowed);
    assert_eq!(
        plan.manifest.run_bindings[0].sidebar_live_threads,
        ["live-thread"]
    );

    // A deleted sidebar thread binds nothing.
    f.tombstone_thread("live-thread");
    assert!(f.preview().apply_allowed);

    // The database binds the run to a live thread.
    let f = F::new();
    f.question("q-db", "run-dead", "run-dead");
    f.sql("INSERT INTO thread_runs(run_id,thread_id,root_run_id,depth) VALUES('run-dead','live-db','run-dead',0)", []);
    f.tombstone_run("run-dead");
    let plan = f.preview();
    assert!(!plan.apply_allowed);
    assert_eq!(
        plan.manifest.run_bindings[0].database_live_threads,
        ["live-db"]
    );
}

#[test]
fn an_unreadable_sidebar_makes_the_plan_unappliable_and_nothing_is_assumed() {
    for (name, content) in [
        ("missing", None),
        ("garbage", Some("not json")),
        ("shape", Some("{\"threads\": 3}")),
        (
            "run_ids",
            Some("{\"threads\":[{\"id\":\"t\",\"run_ids\":[1]}]}"),
        ),
        (
            "run_ids missing",
            Some("{\"threads\":[{\"id\":\"t\",\"root_run_id\":\"r\"}]}"),
        ),
        (
            "run_ids null",
            Some("{\"threads\":[{\"id\":\"t\",\"run_ids\":null}]}"),
        ),
    ] {
        let f = F::standard();
        match content {
            None => fs::remove_file(&f.sidebar).unwrap(),
            Some(text) => fs::write(&f.sidebar, text).unwrap(),
        }
        let plan = f.preview();
        assert!(!plan.apply_allowed, "{name}");
        assert!(
            plan.manifest
                .unprocessable
                .iter()
                .any(|u| u.kind == "sidebar"),
            "{name}"
        );
        let before = f.state();
        let message = error_text(apply_archive_blocker_cleanup(
            &f.options(&f.save(&plan), &f.backup_path()),
        ));
        assert!(message.contains("cannot be processed"), "{name}: {message}");
        assert_eq!(f.state(), before);
    }
}

#[test]
fn lanes_are_excluded_or_unprocessable_for_stated_reasons() {
    let f = F::new();
    f.lane("lane-dead-thread", "dead-lane-thread", "lead", "attached");
    f.lane(
        "lane-dead-lead",
        "live-lane-thread",
        "dead-lead",
        "attached",
    );
    f.lane("lane-broken", "broken-thread", "lead", "attached");
    f.sql(
        "UPDATE lanes SET current_close_id='missing-close' WHERE lane_id='lane-broken'",
        [],
    );
    f.lane("lane-fine", "fine-thread", "lead", "summary");
    f.tombstone_thread("dead-lane-thread");
    f.tombstone_thread("dead-lead");
    let plan = f.preview();
    let excluded: Vec<_> = plan
        .manifest
        .exclusions
        .iter()
        .map(|e| e.id.as_str())
        .collect();
    assert_eq!(excluded, ["lane-dead-thread"]);
    let unprocessable: Vec<_> = plan
        .manifest
        .unprocessable
        .iter()
        .map(|e| e.id.as_str())
        .collect();
    assert_eq!(unprocessable, ["lane-broken", "lane-dead-lead"]);
    assert_eq!(plan.manifest.lanes.len(), 1);
    assert!(!plan.apply_allowed);
}

#[test]
fn a_pending_notification_that_cannot_be_obsoleted_blocks_the_plan() {
    let f = F::new();
    f.question("q1", "run-a", "run-a");
    f.lane("lane-open", "lane-thread", "lead", "attached");
    f.question_notification("n1", "lane-open", "q1", "lane-thread", "lead");
    f.tombstone_thread("lead");
    let plan = f.preview();
    assert!(!plan.apply_allowed);
    assert!(
        plan.manifest
            .unprocessable
            .iter()
            .any(|u| u.kind == "notification" && u.id == "n1")
    );
}

#[test]
fn only_the_current_schema_is_accepted_by_preview_and_apply() {
    let f = F::standard();
    let plan = f.preview();
    let path = f.save(&plan);
    let current: u32 = f
        .conn()
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    for version in [current - 1, current + 1] {
        f.conn()
            .pragma_update(None, "user_version", version)
            .unwrap();
        let preview = preview_archive_blocker_cleanup(&CleanupPreviewOptions {
            db: f.db.clone(),
            config_dir: f.config.clone(),
            sidebar: f.sidebar.clone(),
        });
        let message = preview.unwrap_err().to_string();
        assert!(message.contains("schema"), "{message}");
        let message = error_text(apply_archive_blocker_cleanup(
            &f.options(&path, &f.backup_path()),
        ));
        assert!(message.contains("schema"), "{message}");
    }
    f.conn()
        .pragma_update(None, "user_version", current)
        .unwrap();
    assert!(!f.backup_path().exists());
}

#[test]
fn the_receipt_is_immutable_and_holds_hashes_and_counts_only() {
    let f = F::standard();
    let plan = f.preview();
    apply_archive_blocker_cleanup(&f.options(&f.save(&plan), &f.backup_path())).unwrap();
    let conn = f.conn();
    assert!(
        conn.execute(
            "UPDATE archive_blocker_cleanup_receipts SET closed_lanes=0",
            []
        )
        .is_err()
    );
    assert!(
        conn.execute("DELETE FROM archive_blocker_cleanup_receipts", [])
            .is_err()
    );
    let columns: Vec<String> = conn
        .prepare("SELECT name FROM pragma_table_info('archive_blocker_cleanup_receipts')")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        columns,
        [
            "plan_hash",
            "manifest_sha256",
            "source_sha256",
            "backup_sha256",
            "cancelled_questions",
            "obsoleted_notifications",
            "closed_lanes",
            "completed_at_ns"
        ]
    );
}

#[test]
fn an_old_plan_never_touches_a_question_or_reopened_lane_that_appeared_later() {
    let f = F::standard();
    let plan = f.preview();
    let path = f.save(&plan);
    apply_archive_blocker_cleanup(&f.options(&path, &f.backup_path())).unwrap();
    // A lane reopens (its close pointer clears) and a new question arrives.
    f.sql(
        "UPDATE lanes SET current_close_id=NULL WHERE lane_id='lane-open'",
        [],
    );
    f.question("q-later", "run-later", "run-later");
    let before = f.state();
    let again = apply_archive_blocker_cleanup(&f.options(&path, &f.backup_path())).unwrap();
    assert_eq!(again.status, "already_applied");
    assert_eq!(
        f.state(),
        before,
        "the old plan changed nothing, including the later question and lane"
    );
}
