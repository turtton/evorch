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
    let args = parse(&[
        "maintenance",
        "backfill-events",
        "--batch-rows",
        "2",
        "--max-rows",
        "3",
    ])
    .unwrap();
    assert_eq!(
        args.backfill.unwrap(),
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
        vec!["backfill-events", "--apply"],
        vec!["backfill-events", "--backup", "saved.db"],
        vec!["backfill-events", "--sidebar", "sidebar.json"],
        vec!["backfill-events", "--batch-rows", "0"],
        vec!["backfill-events", "--batch-rows", "100001"],
        vec!["backfill-events", "--db", "a", "--db", "b"],
    ] {
        let full = [vec!["maintenance"], args].concat();
        assert!(parse(&full).is_err(), "{full:?}");
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
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_evorch"))
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
