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
