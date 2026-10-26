//! Offline event repair and read-only retention preview. No runtime is started.

use serde_json::{Value, json};
use std::path::PathBuf;
use storage::maintenance::{
    StripOptions, preview_task_progress_retention, strip_task_progress_cursor,
};

pub const USAGE: &str = "usage: evorch maintenance <strip-task-progress-cursor|preview-task-progress-retention> [--db <path>]
  strip-task-progress-cursor [--apply] [--backup <path> | --no-backup] [--no-vacuum] [--batch-size <1..256>]
Default: read-only dry-run. Stop ALL evorch GUI/runtime processes before either preview or --apply.
Preview requires an existing maintenance lease and a checkpointed DB (no nonempty WAL).
Apply creates a backup, commits bounded batches, then full VACUUM and WAL TRUNCATE.
Full VACUUM can take minutes or longer; time depends on disk throughput and DB size.
Allow up to four times the database size as free space (backup plus scratch).
Keep the backup secure: it includes the old cursor payloads. Never run on a live store.";

#[derive(Debug, Clone)]
pub struct MaintenanceArgs {
    pub db: Option<PathBuf>,
    pub retention_preview: bool,
    pub strip: StripOptions,
}

pub fn parse_args(argv: impl Iterator<Item = String>) -> Result<MaintenanceArgs, String> {
    let mut argv = argv;
    if argv.next().as_deref() != Some("maintenance") {
        return Err(USAGE.into());
    }
    let retention_preview = match argv.next().as_deref() {
        Some("strip-task-progress-cursor") => false,
        Some("preview-task-progress-retention") => true,
        _ => return Err(USAGE.into()),
    };
    let mut args = MaintenanceArgs {
        db: None,
        retention_preview,
        strip: StripOptions::default(),
    };
    let mut seen = std::collections::BTreeSet::new();
    while let Some(arg) = argv.next() {
        if !seen.insert(arg.clone()) {
            return Err(USAGE.into());
        }
        match arg.as_str() {
            "--db" => args.db = Some(argv.next().ok_or(USAGE)?.into()),
            "--apply" if !retention_preview => args.strip.apply = true,
            "--backup" if !retention_preview => {
                args.strip.backup = Some(argv.next().ok_or(USAGE)?.into())
            }
            "--no-backup" if !retention_preview => args.strip.no_backup = true,
            "--no-vacuum" if !retention_preview => args.strip.no_vacuum = true,
            "--batch-size" if !retention_preview => {
                args.strip.batch_size = argv
                    .next()
                    .ok_or(USAGE)?
                    .parse()
                    .ok()
                    .filter(|n| (1..=256).contains(n))
                    .ok_or(USAGE)?;
            }
            _ => return Err(USAGE.into()),
        }
    }
    if args.strip.no_backup && args.strip.backup.is_some() {
        return Err(USAGE.into());
    }
    Ok(args)
}

pub fn run(args: MaintenanceArgs) -> Result<Value, String> {
    let path = match args.db {
        Some(path) => path,
        None => config::user_config_dir()
            .ok_or("cannot resolve config directory; pass --db")?
            .join("evorch-events.db"),
    };
    if args.retention_preview {
        return preview_task_progress_retention(&path)
            .map(|summary| json!(summary))
            .map_err(|error| error.to_string());
    }
    if args.strip.apply {
        let size = std::fs::metadata(&path)
            .map_err(|error| error.to_string())?
            .len();
        eprintln!(
            "Offline maintenance: database {size} bytes. Backup is {}. Full VACUUM is {} (minutes or longer; throughput dependent). Reserve up to {} bytes free for backup and scratch. Committed batches survive interruption; rerun to finish. Close every GUI/runtime first.",
            if args.strip.no_backup {
                "DISABLED explicitly"
            } else {
                "enabled"
            },
            if args.strip.no_vacuum {
                "disabled"
            } else {
                "enabled"
            },
            size.saturating_mul(4)
        );
    }
    strip_task_progress_cursor(&path, &args.strip)
        .map(|summary| json!(summary))
        .map_err(|error| error.to_string())
}
