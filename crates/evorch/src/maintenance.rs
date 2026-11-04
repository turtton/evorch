//! Offline repair, read-only preview and lightweight online backfill. No runtime is started.

use serde_json::{Value, json};
use std::path::PathBuf;
use storage::EventBackfillOptions;
use storage::maintenance::{
    CompactOptions, ConvertBodiesOptions, DeltaMode, StripOptions, VacuumOptions, backfill_events,
    compact_events, convert_bodies, preview_task_progress_retention, strip_task_progress_cursor,
    vacuum_database,
};

pub const USAGE: &str = "usage: evorch maintenance <command> [--db <path>]
  compact-events [--apply] [--backup <new-path>] [--batch-rows <1..100000>]
                 [--provider-retention-days <N>] [--delta-mode <preserve|delete>] [--sidebar <path>]
  backfill-events [--batch-rows <1..100000>] [--max-rows <N>]
  vacuum [--apply] [--backup <new-path>]
  convert-bodies [--apply] [--backup <new-path>] [--event-batch-rows <1..100000>]
  strip-task-progress-cursor [--apply] [--backup <path> | --no-backup] [--no-vacuum] [--batch-size <1..256>]
  preview-task-progress-retention
convert-bodies: one-time conversion of a database of version 33 to 43 to the current format: the conversations in
  run_contexts.messages_json (version 33 to 39; migrations 34 to 39 are applied first, after the backup) become
  run_messages and content_blobs, each provenance map becomes run_message_provenance rows, the two old columns are
  removed, and the long strings of Message, compaction and Tool events become blobs. A regular version 40 to 42
  database (messages already rows; 41 and 42 only add the lane ledger and the memory favorite flag) gets its maps and
  events converted; a version 43 one only its events. Stop ALL evorch
  processes first.
  Default is a read-only preview (needs the .maintenance-lease file next to the database and an empty WAL; a
  non-empty WAL is always refused). --apply takes a verified backup, reclaims rows of deleted runs, converts each
  run in its own transaction after comparing it byte for byte, then each provenance map after comparing it as a typed
  map, then the events in batches (--event-batch-rows rows, at most 16 MiB of payload each) after comparing each as
  a JSON value, then drops the columns, vacuums and publishes the current version. If it stops half way there is no
  resume: restore the backup and start over (the failure report
  lists the steps). A converted database is left untouched by a rerun.
backfill-events: safe next to live writers; converts legacy subkind/run_id columns only.
vacuum: one-time switch of a legacy auto_vacuum=NONE database to INCREMENTAL (compact-events --apply does it too).
  Default is a read-only preview. --apply takes a verified backup, runs one full VACUUM and checks integrity.
  A database that is already INCREMENTAL is left untouched.
Use --max-rows for bounded work. Long-lived readers can pin the WAL and let it grow.
Other commands: default read-only preview. Stop ALL evorch GUI/runtime processes for preview AND apply.
Preview requires an existing maintenance lease and a checkpointed DB (no nonempty WAL).
compact-events --apply: mandatory verified backup, restartable batches, accounting repair, VACUUM.
Default preserves message/reasoning display anchors; existing single oversized rows are reported, not split.
--sidebar defaults to the GUI's <user-config-dir>/sidebar.json. Unreadable/missing/invalid sidebar disables delta aggregation with a warning.
--delta-mode delete opts into deleting ALL message/reasoning deltas, including incomplete text.
--provider-retention-days defaults to 90; 0 disables it. RequestStarted is always retained.
Apply creates a backup, commits bounded batches, then full VACUUM and WAL TRUNCATE.
Full VACUUM can take minutes or longer; time depends on disk throughput and DB size.
Allow up to four times the database size as free space (backup plus scratch).
Keep backups secure: they include the original payloads. Never run offline commands on a live store.";

#[derive(Debug, Clone)]
pub struct MaintenanceArgs {
    pub db: Option<PathBuf>,
    pub retention_preview: bool,
    pub strip: StripOptions,
    pub compact: Option<CompactOptions>,
    pub backfill: Option<EventBackfillOptions>,
    pub vacuum: Option<VacuumOptions>,
    pub convert_bodies: Option<ConvertBodiesOptions>,
}

pub fn parse_args(argv: impl Iterator<Item = String>) -> Result<MaintenanceArgs, String> {
    let mut argv = argv;
    if argv.next().as_deref() != Some("maintenance") {
        return Err(USAGE.into());
    }
    let command = argv.next().ok_or(USAGE)?;
    let retention_preview = match command.as_str() {
        "strip-task-progress-cursor"
        | "compact-events"
        | "backfill-events"
        | "vacuum"
        | "convert-bodies" => false,
        "preview-task-progress-retention" => true,
        _ => return Err(USAGE.into()),
    };
    let mut args = MaintenanceArgs {
        db: None,
        retention_preview,
        strip: StripOptions::default(),
        compact: (command == "compact-events").then(CompactOptions::default),
        backfill: (command == "backfill-events").then(EventBackfillOptions::default),
        vacuum: (command == "vacuum").then(VacuumOptions::default),
        convert_bodies: (command == "convert-bodies").then(ConvertBodiesOptions::default),
    };
    let mut seen = std::collections::BTreeSet::new();
    while let Some(arg) = argv.next() {
        if !seen.insert(arg.clone()) {
            return Err(USAGE.into());
        }
        match arg.as_str() {
            "--db" => args.db = Some(argv.next().ok_or(USAGE)?.into()),
            "--apply" if args.compact.is_some() => args.compact.as_mut().unwrap().apply = true,
            "--backup" if args.compact.is_some() => {
                args.compact.as_mut().unwrap().backup = Some(argv.next().ok_or(USAGE)?.into());
            }
            "--apply" if args.vacuum.is_some() => args.vacuum.as_mut().unwrap().apply = true,
            "--backup" if args.vacuum.is_some() => {
                args.vacuum.as_mut().unwrap().backup = Some(argv.next().ok_or(USAGE)?.into());
            }
            "--apply" if args.convert_bodies.is_some() => {
                args.convert_bodies.as_mut().unwrap().apply = true;
            }
            "--event-batch-rows" if args.convert_bodies.is_some() => {
                args.convert_bodies.as_mut().unwrap().event_batch_rows = Some(
                    argv.next()
                        .ok_or(USAGE)?
                        .parse()
                        .ok()
                        .filter(|n| (1..=100_000).contains(n))
                        .ok_or(USAGE)?,
                );
            }
            "--backup" if args.convert_bodies.is_some() => {
                args.convert_bodies.as_mut().unwrap().backup =
                    Some(argv.next().ok_or(USAGE)?.into());
            }
            "--sidebar" if args.compact.is_some() => {
                args.compact.as_mut().unwrap().sidebar = Some(argv.next().ok_or(USAGE)?.into());
            }
            "--batch-rows" if args.compact.is_some() || args.backfill.is_some() => {
                let n: usize = argv
                    .next()
                    .ok_or(USAGE)?
                    .parse()
                    .ok()
                    .filter(|n| (1..=100_000).contains(n))
                    .ok_or(USAGE)?;
                if let Some(options) = &mut args.compact {
                    options.batch_rows = n;
                }
                if let Some(options) = &mut args.backfill {
                    options.batch_rows = n;
                }
            }
            "--max-rows" if args.backfill.is_some() => {
                args.backfill.as_mut().unwrap().max_rows =
                    Some(argv.next().ok_or(USAGE)?.parse().map_err(|_| USAGE)?);
            }
            "--provider-retention-days" if args.compact.is_some() => {
                let days: u64 = argv.next().ok_or(USAGE)?.parse().map_err(|_| USAGE)?;
                days.checked_mul(86_400).ok_or(USAGE)?;
                args.compact.as_mut().unwrap().provider_retention_days = days;
            }
            "--delta-mode" if args.compact.is_some() => {
                args.compact.as_mut().unwrap().delta_mode = match argv.next().as_deref() {
                    Some("preserve") => DeltaMode::Preserve,
                    Some("delete") => DeltaMode::Delete,
                    _ => return Err(USAGE.into()),
                };
            }
            "--apply" if command == "strip-task-progress-cursor" => args.strip.apply = true,
            "--backup" if command == "strip-task-progress-cursor" => {
                args.strip.backup = Some(argv.next().ok_or(USAGE)?.into())
            }
            "--no-backup" if command == "strip-task-progress-cursor" => args.strip.no_backup = true,
            "--no-vacuum" if command == "strip-task-progress-cursor" => args.strip.no_vacuum = true,
            "--batch-size" if command == "strip-task-progress-cursor" => {
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
    if let Some(options) = args.backfill {
        return backfill_events(&path, &options)
            .map(|summary| json!(summary))
            .map_err(|error| error.to_string());
    }
    if let Some(options) = args.convert_bodies {
        if options.apply {
            eprintln!(
                "Offline conversion: stop ALL evorch processes. Mandatory verified backup; reserve four times the database size. \
                 A stop half way has no resume: restore the backup and start over. Backup, per-run checks and VACUUM can take minutes or longer."
            );
        }
        let summary = convert_bodies(&path, &options).map_err(|error| error.to_string())?;
        eprintln!("{}", summary.message);
        return Ok(json!(summary));
    }
    if let Some(options) = args.vacuum {
        if options.apply {
            eprintln!(
                "Offline vacuum: stop ALL evorch processes. Mandatory verified backup; reserve four times the database size. Full VACUUM may take minutes or longer."
            );
        }
        let summary = vacuum_database(&path, &options).map_err(|error| error.to_string())?;
        eprintln!("{}", summary.message);
        return Ok(json!(summary));
    }
    if let Some(mut options) = args.compact {
        if options.sidebar.is_none() {
            options.sidebar = config::user_config_dir().map(|dir| dir.join("sidebar.json"));
        }
        if options.delta_mode == DeltaMode::Delete {
            eprintln!(
                "WARNING: --delta-mode delete removes reasoning display and incomplete message text. Completed messages remain. This is irreversible without restoring the backup."
            );
        }
        if options.apply {
            eprintln!(
                "Offline compaction: stop ALL evorch processes. Mandatory verified backup; reserve four times the database size. Committed groups survive interruption; rerun to finish. Full VACUUM may take minutes or longer."
            );
        }
        return compact_events(&path, &options)
            .map(|summary| json!(summary))
            .map_err(|error| error.to_string());
    }
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
