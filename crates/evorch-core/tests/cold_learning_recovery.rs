#![cfg(target_os = "linux")]
//! Cold startup must fence bad learning components without losing healthy work.
use core_instance::ConfigIdentity;
use std::{
    collections::BTreeSet,
    io::Write,
    sync::{Arc, Mutex},
};
use storage::{Database, Storage, StorageConfig, learning_jobs::*, recovery::restart::*};
use tracing::instrument::WithSubscriber;
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost};

#[derive(Clone, Default)]
struct Diagnostics(Arc<Mutex<Vec<u8>>>);
impl Write for Diagnostics {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn origins(sql: &rusqlite::Connection) -> Vec<Vec<String>> {
    sql.prepare(
        "SELECT task_id, quote(origin_kind), quote(origin_instance), quote(origin_pid),
         quote(origin_start_ticks), quote(origin_boot_id), quote(origin_pid_ns), quote(config_identity)
         FROM learning_jobs ORDER BY task_id",
    )
    .unwrap()
    .query_map([], |row| (0..8).map(|i| row.get(i)).collect())
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

fn cohort(writer: &storage::StorageHandle, db: &Database, id: &str, origin: &LearningOrigin) {
    let mut tx = writer
        .mutate_restart(
            id.into(),
            None,
            RestartMutation::Create(Box::new(RestartMetadata {
                restart_id: id.into(),
                capture_id: format!("capture/{id}"),
                config_identity: origin.config_identity.clone().unwrap(),
                old_epoch: 1,
                successor_epoch: 2,
                old_build: "old".into(),
                successor_build: "new".into(),
                target_identity: "target".into(),
                handoff_version: 1,
                snapshot_version: 1,
                storage_version: 62,
                old_builtin_tool_names: BTreeSet::new(),
                old_compiled_catalog_fingerprint: None,
                new_compiled_catalog_fingerprint: None,
                old_effective_tools_fingerprint: None,
                new_effective_tools_fingerprint: None,
            })),
        )
        .unwrap();
    tx = writer
        .mutate_restart(
            id.into(),
            Some(tx.revision),
            RestartMutation::BeginCapture(vec![]),
        )
        .unwrap();
    let inventory = db
        .restart_learning_inventory(origin.clone(), "suspended".into())
        .unwrap();
    writer
        .mutate_restart(
            id.into(),
            Some(tx.revision),
            RestartMutation::LearningInventory(inventory),
        )
        .unwrap();
}

async fn cold_start_with_corruption(corruption: &str) {
    let data = tempfile::tempdir().unwrap();
    let owners = test_socket_dir::SocketDir::new();
    let config_dir = data.path().join("config");
    let project = data.path().join("project");
    std::fs::create_dir(&config_dir).unwrap();
    std::fs::create_dir(&project).unwrap();
    let config = StorageConfig {
        db_path: config_dir.join("evorch-events.db"),
        ..Default::default()
    };
    let store = Storage::open(config.clone()).unwrap();
    let writer = store.handle();
    let db = Database::open(&config).unwrap();
    let mut origin = runtime::learning_jobs::current_origin(
        LearningOriginKind::Core,
        Some(ConfigIdentity::new(&config_dir).unwrap().identity().into()),
    );
    // Same process identity under an earlier image is definitively dead, like exec.
    origin.instance = "predecessor".into();
    for id in [
        "cohort-bad",
        "cohort-good",
        "unbound-bad",
        "unbound-good",
        "unrelated",
    ] {
        let mut who = origin.clone();
        who.instance = id.into();
        if id == "unrelated" {
            who.kind = LearningOriginKind::Gui;
            who.config_identity = None;
        }
        writer
            .admit_learning_job(
                NewLearningJob {
                    task_id: id.into(),
                    roots: vec![id.into()],
                    goal: false,
                    daily_limit: 20,
                    project: project.display().to_string(),
                    source_run_id: id.into(),
                    origin: who.clone(),
                    prompt: "private evidence".into(),
                    evidence_json: "{}".into(),
                },
                std::time::SystemTime::now(),
            )
            .unwrap();
        for change in [
            LearningAttemptChange::Start,
            LearningAttemptChange::Interrupt {
                suspension_id: "suspended".into(),
            },
        ] {
            assert!(
                writer
                    .write_learning_attempt(
                        LearningAttemptWrite {
                            key: db.learning_job(id).unwrap().unwrap().key(),
                            change,
                        },
                        std::time::SystemTime::now()
                    )
                    .unwrap()
            );
        }
        if id.starts_with("cohort-") {
            cohort(&writer, &db, id, &who);
        }
    }
    let sql = rusqlite::Connection::open(&config.db_path).unwrap();
    // Model externally damaged storage, then restore normal write fences before boot.
    let fence: String = sql
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name='learning_job_forward_only'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    sql.execute_batch(
        "DROP TRIGGER learning_job_forward_only; PRAGMA ignore_check_constraints=ON;",
    )
    .unwrap();
    // Outside Interrupted: must be filtered before any origin decoding.
    sql.execute(
        "UPDATE learning_jobs SET state='Finished', origin_pid=-1 WHERE task_id='unrelated'",
        [],
    )
    .unwrap();
    match corruption {
        "pid" => {
            sql.execute("UPDATE learning_jobs SET origin_pid=-1 WHERE task_id IN ('cohort-bad','unbound-bad')", []).unwrap();
        }
        "type" => {
            sql.execute("UPDATE learning_jobs SET origin_kind=x'00' WHERE task_id IN ('cohort-bad','unbound-bad')", []).unwrap();
        }
        "inventory-type" | "member-type" => {
            let statement = if corruption == "inventory-type" {
                "UPDATE restart_learning_inventory SET inventory_json=x'00' WHERE restart_id='cohort-bad'"
            } else {
                "UPDATE restart_learning_members SET member_json=x'00' WHERE restart_id='cohort-bad'"
            };
            sql.execute(statement, []).unwrap();
            sql.execute(
                "UPDATE learning_jobs SET origin_pid=-1 WHERE task_id='unbound-bad'",
                [],
            )
            .unwrap();
        }
        "json" => {
            sql.execute("UPDATE restart_learning_inventory SET inventory_json='{' WHERE restart_id='cohort-bad'", []).unwrap();
            sql.execute(
                "UPDATE learning_jobs SET origin_kind='invalid' WHERE task_id='unbound-bad'",
                [],
            )
            .unwrap();
        }
        _ => unreachable!(),
    }
    sql.execute_batch(&fence).unwrap();
    sql.execute_batch("PRAGMA ignore_check_constraints=OFF;")
        .unwrap();
    let before = origins(&sql);
    drop(db);
    drop(writer);
    drop(store);
    let diagnostics = Diagnostics::default();
    let sink = diagnostics.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(move || sink.clone())
        .finish();
    // open_for_core awaits CoreGraphRecovery's ready oneshot before returning;
    // this is startup completion, not a polling interval or a wall-clock deadline.
    let mut host = BootstrappedHost::open_for_core(
        BootstrapOptions {
            config_dir,
            project_root: project,
            ownership_root: owners.path().into(),
        },
        &evorch_core::recovery_service::CoreGraphRecovery,
    )
    .with_subscriber(subscriber)
    .await
    .unwrap();
    assert!(
        host.recovery_diagnostics.is_some(),
        "cold startup reached its Ready boundary"
    );
    let db = Database::open(&config).unwrap();
    for id in ["cohort-good", "unbound-good"] {
        assert_eq!(
            db.learning_job(id).unwrap().unwrap().state,
            LearningJobState::Abandoned,
            "{id}"
        );
        assert!(db.learning_job_evidence(id).unwrap().is_none());
    }
    assert_eq!(
        db.restart_learning_members("cohort-good").unwrap()[0].disposition,
        "quarantined"
    );
    for id in ["cohort-bad", "unbound-bad", "unrelated"] {
        let state: String = sql
            .query_row(
                "SELECT state FROM learning_jobs WHERE task_id=?1",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            state,
            if id == "unrelated" {
                "Finished"
            } else {
                "Interrupted"
            }
        );
        assert!(
            db.learning_job_evidence(id).unwrap().is_some(),
            "isolation retains evidence"
        );
    }
    let disposition: String = sql
        .query_row(
            "SELECT disposition FROM restart_learning_members WHERE restart_id='cohort-bad'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        disposition, "pending",
        "corrupt cohort grants no cleanup authority"
    );
    let after = origins(&sql);
    assert_eq!(after, before, "never guess or rewrite origin");
    let log = String::from_utf8(diagnostics.0.lock().unwrap().clone()).unwrap();
    assert!(
        log.contains("cohort-bad"),
        "cohort diagnostic missing: {log}"
    );
    assert!(log.contains("unbound-bad"), "job diagnostic missing: {log}");
    assert!(
        !log.contains("unrelated"),
        "non-candidate was decoded: {log}"
    );
    assert!(!log.contains("private evidence"));
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn cold_ready_isolates_out_of_range_learning_jobs() {
    cold_start_with_corruption("pid").await;
}
#[tokio::test]
async fn cold_ready_isolates_sql_column_type_errors() {
    for corruption in ["type", "inventory-type", "member-type"] {
        cold_start_with_corruption(corruption).await;
    }
}
#[tokio::test]
async fn cold_ready_isolates_learning_json_errors() {
    cold_start_with_corruption("json").await;
}

#[tokio::test]
async fn cold_start_propagates_learning_database_failure() {
    let data = tempfile::tempdir().unwrap();
    let owners = test_socket_dir::SocketDir::new();
    let config_dir = data.path().join("config");
    let project_root = data.path().join("project");
    std::fs::create_dir(&config_dir).unwrap();
    std::fs::create_dir(&project_root).unwrap();
    let config = StorageConfig {
        db_path: config_dir.join("evorch-events.db"),
        ..Default::default()
    };
    drop(Storage::open(config.clone()).unwrap());
    rusqlite::Connection::open(&config.db_path)
        .unwrap()
        .execute_batch("ALTER TABLE learning_jobs RENAME TO unavailable_jobs")
        .unwrap();
    let error = BootstrappedHost::open_for_core(
        BootstrapOptions {
            config_dir,
            project_root,
            ownership_root: owners.path().into(),
        },
        &evorch_core::recovery_service::CoreGraphRecovery,
    )
    .await
    .err()
    .expect("database failure cannot be treated as an isolated learning row");
    assert!(
        error.to_string().contains("no such table: learning_jobs"),
        "{error}"
    );
}
