use evorch::maintenance::parse_args;

fn parse(args: &[&str]) -> Result<evorch::maintenance::MaintenanceArgs, String> {
    parse_args(args.iter().map(|arg| (*arg).into()))
}

#[test]
fn maintenance_defaults_are_safe_and_apply_is_explicit() {
    let args = parse(&["maintenance", "strip-task-progress-cursor"]).unwrap();
    assert!(!args.strip.apply && !args.strip.no_backup && !args.strip.no_vacuum);
    assert!(
        parse(&["maintenance", "strip-task-progress-cursor", "--apply"])
            .unwrap()
            .strip
            .apply
    );
    assert!(parse(&["maintenance", "preview-task-progress-retention", "--apply"]).is_err());
    assert!(
        parse(&[
            "maintenance",
            "strip-task-progress-cursor",
            "--batch-size",
            "0"
        ])
        .is_err()
    );
    assert!(
        parse(&[
            "maintenance",
            "strip-task-progress-cursor",
            "--batch-size",
            "257"
        ])
        .is_err()
    );
    assert!(
        parse(&[
            "maintenance",
            "strip-task-progress-cursor",
            "--backup",
            "backup.db",
            "--no-backup"
        ])
        .is_err()
    );
}

#[test]
fn compact_and_online_backfill_have_separate_safety_options() {
    use storage::maintenance::DeltaMode;
    let args = parse(&["maintenance", "compact-events"]).unwrap();
    let options = args.compact.unwrap();
    assert!(!options.apply);
    assert_eq!(options.delta_mode, DeltaMode::Preserve);
    assert_eq!(options.provider_retention_days, 90);
    let args = parse(&[
        "maintenance",
        "compact-events",
        "--apply",
        "--delta-mode",
        "delete",
        "--provider-retention-days",
        "0",
        "--backup",
        "saved.db",
        "--batch-rows",
        "17",
        "--sidebar",
        "saved-sidebar.json",
    ])
    .unwrap();
    let options = args.compact.unwrap();
    assert!(options.apply);
    assert_eq!(options.delta_mode, DeltaMode::Delete);
    assert_eq!(options.provider_retention_days, 0);
    assert_eq!(options.batch_rows, 17);
    assert_eq!(options.sidebar, Some("saved-sidebar.json".into()));
    let args = evorch::backfill::parse_args(
        [
            "maintenance",
            "backfill-events",
            "--batch-rows",
            "2",
            "--max-rows",
            "3",
        ]
        .into_iter()
        .map(String::from),
    )
    .unwrap();
    assert_eq!(
        args.options,
        storage::EventBackfillOptions {
            batch_rows: 2,
            max_rows: Some(3)
        }
    );
    for args in [
        vec!["compact-events", "--no-backup"],
        vec!["compact-events", "--no-vacuum"],
        vec!["compact-events", "--max-rows", "1"],
        vec!["compact-events", "--delta-mode", "unknown"],
        vec!["compact-events", "--provider-retention-days", "-1"],
    ] {
        let full = [vec!["maintenance"], args].concat();
        assert!(parse(&full).is_err(), "{full:?}");
    }
    // The online backfill has a parser of its own and takes only its own options;
    // the offline one no longer knows it.
    assert!(parse(&["maintenance", "backfill-events"]).is_err());
    for args in [
        vec!["backfill-events", "--apply"],
        vec!["backfill-events", "--backup", "saved.db"],
        vec!["backfill-events", "--sidebar", "sidebar.json"],
        vec!["backfill-events", "--batch-rows", "0"],
        vec!["backfill-events", "--batch-rows", "100001"],
        vec!["backfill-events", "--db", "a", "--db", "b"],
    ] {
        let full = [vec!["maintenance"], args].concat();
        assert!(
            evorch::backfill::parse_args(full.iter().map(|arg| (*arg).into())).is_err(),
            "{full:?}"
        );
    }
}

#[test]
fn vacuum_is_a_preview_until_apply_and_takes_only_apply_and_backup() {
    let options = parse(&["maintenance", "vacuum"]).unwrap().vacuum.unwrap();
    assert!(!options.apply && options.backup.is_none());
    let options = parse(&["maintenance", "vacuum", "--apply", "--backup", "saved.db"])
        .unwrap()
        .vacuum
        .unwrap();
    assert!(options.apply);
    assert_eq!(options.backup, Some("saved.db".into()));
    for args in [
        vec!["vacuum", "--no-backup"],
        vec!["vacuum", "--batch-rows", "1"],
        vec!["vacuum", "--apply", "--apply"],
        vec!["compact-events", "--vacuum"],
    ] {
        let full = [vec!["maintenance"], args].concat();
        assert!(parse(&full).is_err(), "{full:?}");
    }
}

#[test]
fn backfill_cli_outputs_progress_without_starting_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.db");
    let db = storage::Database::open(&storage::StorageConfig {
        db_path: path.clone(),
        ..Default::default()
    })
    .unwrap();
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute(
        "INSERT INTO events(schema_version,monotonic_ns,wall_clock_ns,kind,payload) VALUES(1,0,0,'Message','undecodable')",
        [],
    ).unwrap();
    let result = std::process::Command::new(
        std::env::var("CARGO_BIN_EXE_evorch").expect("CARGO_BIN_EXE_evorch"),
    )
    .args([
        "maintenance",
        "backfill-events",
        "--db",
        path.to_str().unwrap(),
        "--batch-rows",
        "1",
        "--max-rows",
        "1",
    ])
    .output()
    .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let summary: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(
        summary,
        serde_json::json!({"converted":1,"reattributed":0,"undecodable":1,"batches":1,"remaining":0})
    );
    drop(db);
}

#[test]
fn convert_bodies_is_a_preview_until_apply_and_takes_only_apply_backup_and_event_batch_rows() {
    let options = parse(&["maintenance", "convert-bodies"])
        .unwrap()
        .convert_bodies
        .unwrap();
    assert!(!options.apply && options.backup.is_none());
    let options = parse(&[
        "maintenance",
        "convert-bodies",
        "--apply",
        "--backup",
        "saved.db",
    ])
    .unwrap()
    .convert_bodies
    .unwrap();
    assert!(options.apply);
    assert_eq!(options.backup, Some("saved.db".into()));
    assert_eq!(options.event_batch_rows, None);
    let options = parse(&["maintenance", "convert-bodies", "--event-batch-rows", "250"])
        .unwrap()
        .convert_bodies
        .unwrap();
    assert_eq!(options.event_batch_rows, Some(250));
    for args in [
        vec!["convert-bodies", "--event-batch-rows", "0"],
        vec!["convert-bodies", "--event-batch-rows", "100001"],
        vec!["backfill-events", "--event-batch-rows", "10"],
        vec!["convert-bodies", "--resume"],
        vec!["convert-bodies", "--no-backup"],
        vec!["convert-bodies", "--batch-rows", "1"],
        vec!["convert-bodies", "--apply", "--apply"],
        vec!["vacuum", "--resume"],
    ] {
        let full = [vec!["maintenance"], args].concat();
        assert!(parse(&full).is_err(), "{full:?}");
    }
}

#[test]
fn convert_bodies_cli_leaves_a_converted_database_alone_and_refuses_a_non_empty_wal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.db");
    let db = storage::Database::open(&storage::StorageConfig {
        db_path: path.clone(),
        ..Default::default()
    })
    .unwrap();
    drop(db);
    std::fs::File::create(dir.path().join("events.db.maintenance-lock")).unwrap();
    let run = |extra: &[&str]| {
        std::process::Command::new(
            std::env::var("CARGO_BIN_EXE_evorch").expect("CARGO_BIN_EXE_evorch"),
        )
        .args([
            "maintenance",
            "convert-bodies",
            "--db",
            path.to_str().unwrap(),
        ])
        .args(extra)
        .output()
        .unwrap()
    };

    // Given/When: a database already in the current format, previewed and applied.
    for extra in [&[][..], &["--apply"][..]] {
        let result = run(extra);

        // Then: the JSON report says so, and no backup is made.
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let summary: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(summary["already_converted"], true);
        assert_eq!(
            summary["scope"],
            "run_contexts, run_context_provenance, events"
        );
        assert!(summary["backup"].is_null());
    }

    // Given: a log that holds a committed write.
    let writer = rusqlite::Connection::open(&path).unwrap();
    writer.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    writer
        .execute(
            "INSERT INTO retention_identity(id, db_id) VALUES (1, 'x')",
            [],
        )
        .unwrap();

    // When/Then: both modes fail with the refusal and say that the database is unchanged.
    for extra in [&[][..], &["--apply"][..]] {
        let result = run(extra);
        assert!(!result.status.success());
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(stderr.contains("non-empty WAL"), "{stderr}");
        assert!(stderr.contains("the database was not changed"), "{stderr}");
        assert!(!stderr.contains("the database was changed"), "{stderr}");
        assert!(
            !stderr.contains("recover only from the verified backup"),
            "{stderr}"
        );
    }
}

// Offline maintenance next to a running core (ADR 0045): a core holds the database lease
// shared for as long as its writer lives (`shared_with_wait` is what the host takes), and
// every offline command must refuse to start while that lease is held, change nothing, and
// create nothing (no backup, no extra lock file).
mod live_core {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use storage::{Storage, StorageConfig, shared_with_wait, try_exclusive};

    use super::parse;

    struct Place {
        _dir: tempfile::TempDir,
        root: PathBuf,
        db: PathBuf,
    }

    /// A current-format database whose maintenance lease file already exists, as it does
    /// once any current evorch has run on it.
    fn place() -> Place {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let db = root.join("events.db");
        Storage::open(StorageConfig {
            db_path: db.clone(),
            ..Default::default()
        })
        .unwrap()
        .close();
        // The lease is released again; its file stays (it is never unlinked).
        assert!(db.with_extension("db.maintenance-lock").exists());
        Place {
            root: root.join("ownership"),
            db,
            _dir: dir,
        }
    }

    /// Everything the directory holds, with the bytes of every file.
    fn snapshot(dir: &Path) -> Vec<(String, Vec<u8>)> {
        let mut files: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                let bytes = std::fs::read(entry.path()).unwrap_or_default();
                (entry.file_name().to_string_lossy().into_owned(), bytes)
            })
            .collect();
        files.sort();
        files
    }

    /// Every offline command, in both modes where it has two.
    fn commands() -> Vec<Vec<&'static str>> {
        vec![
            vec!["strip-task-progress-cursor"],
            vec!["strip-task-progress-cursor", "--apply"],
            vec!["strip-task-progress-cursor", "--apply", "--no-backup"],
            vec!["preview-task-progress-retention"],
            vec!["compact-events"],
            vec!["compact-events", "--apply"],
            vec!["vacuum"],
            vec!["vacuum", "--apply"],
            vec!["convert-bodies"],
            vec!["convert-bodies", "--apply"],
        ]
    }

    fn refused(db: &Path, command: &[&str]) -> String {
        let mut argv = vec!["maintenance"];
        argv.extend(command);
        argv.extend(["--db", db.to_str().unwrap()]);
        evorch::maintenance::run(parse(&argv).unwrap()).expect_err(&format!("{command:?}"))
    }

    // Given: a core holding the lease / When: any offline command targets the database,
    // under its own name or an alias / Then: it is refused before it touches anything.
    #[test]
    fn every_offline_command_is_refused_while_a_core_holds_the_lease() {
        let place = place();
        let dir = place.db.parent().unwrap().to_path_buf();
        let alias_dir = tempfile::tempdir().unwrap();
        let link = alias_dir.path().join("link.db");
        std::os::unix::fs::symlink(&place.db, &link).unwrap();
        let dir_link = alias_dir.path().join("dir");
        std::os::unix::fs::symlink(&dir, &dir_link).unwrap();
        let dotted = dir.join(".").join("..").join(dir.file_name().unwrap());
        let aliases = [
            place.db.clone(),
            link,
            dir_link.join("events.db"),
            dotted.join("events.db"),
        ];

        let core = shared_with_wait(&place.db, &place.root, Duration::ZERO).unwrap();
        let before = snapshot(&dir);
        for alias in &aliases {
            for command in commands() {
                let error = refused(alias, &command);
                assert!(
                    error.contains("in use") || error.contains("offline maintenance"),
                    "{command:?} via {}: {error}",
                    alias.display()
                );
            }
        }
        assert_eq!(
            snapshot(&dir),
            before,
            "a refused command changed the directory"
        );
        drop(core);
    }

    // Given: the core exited / When: the same commands run / Then: the lease is free for
    // them (a refusal above was the core's, not a leftover), and a refused or failed
    // command leaves no lease behind.
    #[test]
    fn a_refusal_or_a_failure_leaves_no_lease_behind() {
        let place = place();
        let core = shared_with_wait(&place.db, &place.root, Duration::ZERO).unwrap();
        refused(&place.db, &["strip-task-progress-cursor", "--apply"]);
        drop(core);
        assert!(try_exclusive(&place.db, &place.root).unwrap().is_some());

        // A preview that succeeds, and an apply that fails after the lease was taken
        // (the file is not a database): both release it.
        let argv = ["maintenance", "preview-task-progress-retention", "--db"];
        let db = place.db.to_str().unwrap();
        evorch::maintenance::run(parse(&[argv[0], argv[1], argv[2], db]).unwrap()).unwrap();
        assert!(try_exclusive(&place.db, &place.root).unwrap().is_some());

        let broken = place.db.with_file_name("broken.db");
        std::fs::write(&broken, b"not a database, not a database, not a database").unwrap();
        let before = std::fs::read(&broken).unwrap();
        for command in [
            &["vacuum", "--apply"][..],
            &["strip-task-progress-cursor", "--apply"],
            &["compact-events", "--apply"],
            &["convert-bodies", "--apply"],
        ] {
            let mut argv = vec!["maintenance"];
            argv.extend(command);
            argv.extend(["--db", broken.to_str().unwrap()]);
            assert!(
                evorch::maintenance::run(parse(&argv).unwrap()).is_err(),
                "{command:?}"
            );
            assert_eq!(std::fs::read(&broken).unwrap(), before, "{command:?}");
            assert!(
                try_exclusive(&broken, &place.root).unwrap().is_some(),
                "{command:?} kept the lease"
            );
        }
    }

    // The binary, not only the library: exit status 1, an error on stderr, no JSON.
    #[test]
    fn the_command_line_refuses_with_exit_status_1_and_no_json() {
        let place = place();
        let core = shared_with_wait(&place.db, &place.root, Duration::ZERO).unwrap();
        let before = snapshot(place.db.parent().unwrap());
        let output = std::process::Command::new(
            std::env::var("CARGO_BIN_EXE_evorch").expect("CARGO_BIN_EXE_evorch"),
        )
        .args(["maintenance", "vacuum", "--apply", "--db"])
        .arg(&place.db)
        .output()
        .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
        assert_eq!(snapshot(place.db.parent().unwrap()), before);
        drop(core);
    }

    /// A named pipe: opening it for reading or writing waits for the other side.
    fn fifo(dir: &Path) -> PathBuf {
        let path = dir.join("sidebar.fifo");
        let status = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success(), "mkfifo");
        path
    }

    /// `compact-events --apply` in a child process, stopped inside its critical section: with
    /// the lease and SQLite's exclusive lock taken, the command reads the GUI sidebar, and the
    /// sidebar is a named pipe nobody has written yet. Opening the pipe for writing returns
    /// exactly when the child has opened it for reading, so there is no timing assumption:
    /// the child is past both locks and blocked until the returned writer is closed.
    fn apply_blocked_in_its_critical_section(
        place: &Place,
    ) -> (std::process::Child, std::fs::File, PathBuf) {
        let dir = place.db.parent().unwrap();
        let pipe = fifo(dir);
        let backup_dir = tempfile::tempdir().unwrap().keep();
        let child = std::process::Command::new(
            std::env::var("CARGO_BIN_EXE_evorch").expect("CARGO_BIN_EXE_evorch"),
        )
        .args(["maintenance", "compact-events", "--apply", "--db"])
        .arg(&place.db)
        .arg("--sidebar")
        .arg(&pipe)
        .arg("--backup")
        .arg(backup_dir.join("backup.db"))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
        let writer = std::fs::OpenOptions::new().write(true).open(&pipe).unwrap();
        (child, writer, backup_dir)
    }

    /// What every other party sees while the apply is inside its critical section.
    fn assert_everyone_else_is_kept_out(place: &Place) {
        assert!(
            shared_with_wait(&place.db, &place.root, Duration::ZERO).is_err(),
            "a core could start next to a running maintenance"
        );
        assert!(
            Storage::open(StorageConfig {
                db_path: place.db.clone(),
                ..Default::default()
            })
            .is_err(),
            "a writer could open the database during maintenance"
        );
        assert!(try_exclusive(&place.db, &place.root).unwrap().is_none());
        refused(&place.db, &["vacuum", "--apply"]);
        let inspected = evorch::inspect::run(
            evorch::inspect::parse_args(
                ["inspect", "--db", place.db.to_str().unwrap(), "storage"]
                    .into_iter()
                    .map(String::from),
            )
            .unwrap(),
        );
        assert!(inspected.unwrap_err().contains("offline maintenance"));
        let reader = rusqlite::Connection::open(&place.db).unwrap();
        reader.busy_timeout(Duration::ZERO).unwrap();
        assert!(
            reader
                .query_row("SELECT COUNT(*) FROM events", [], |row| row
                    .get::<_, i64>(0))
                .is_err(),
            "SQLite let another process read during the exclusive apply"
        );
    }

    // Given: an apply stopped in its critical section / When: a core, a writer, another
    // offline command, an inspection and a plain reader try to get in / Then: all are kept
    // out; and when the apply finishes the lease is free again and the work is done.
    #[test]
    fn an_apply_in_progress_keeps_every_writer_out_until_it_ends() {
        let place = place();
        let (child, writer, backup_dir) = apply_blocked_in_its_critical_section(&place);
        assert_everyone_else_is_kept_out(&place);

        drop(writer);
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(backup_dir.join("backup.db").exists());
        let lease = try_exclusive(&place.db, &place.root).unwrap();
        assert!(lease.is_some(), "the finished apply still holds the lease");
        drop(lease);
        Storage::open(StorageConfig {
            db_path: place.db.clone(),
            ..Default::default()
        })
        .unwrap()
        .close();
    }

    // Given: an apply stopped in its critical section / When: the process is killed /
    // Then: the operating system releases the lease and SQLite's lock with it, the
    // database is exactly as it was and passes its integrity check, and a writer can open it.
    #[test]
    fn a_killed_apply_releases_the_lease_and_leaves_the_database_as_it_was() {
        let place = place();
        let before = std::fs::read(&place.db).unwrap();
        let (mut child, writer, _backup_dir) = apply_blocked_in_its_critical_section(&place);
        assert_everyone_else_is_kept_out(&place);

        child.kill().unwrap();
        child.wait().unwrap();
        drop(writer);

        let lease = try_exclusive(&place.db, &place.root).unwrap();
        assert!(lease.is_some(), "the killed apply left the lease behind");
        drop(lease);
        let connection = rusqlite::Connection::open(&place.db).unwrap();
        let integrity: String = connection
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .unwrap();
        assert_eq!(integrity, "ok");
        drop(connection);
        Storage::open(StorageConfig {
            db_path: place.db.clone(),
            ..Default::default()
        })
        .unwrap()
        .close();
        assert!(
            std::fs::read(&place.db).unwrap() == before,
            "the killed apply changed the database file"
        );
    }
}
