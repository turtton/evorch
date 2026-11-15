//! The online `evorch maintenance backfill-events`: converts the legacy
//! subkind/run_id columns of the events table in bounded batches, next to live
//! writers. No runtime is started. It is the only maintenance command that may
//! run while a core holds the database, so it has a parser of its own: the
//! offline commands are in [`crate::maintenance`].

use serde_json::{Value, json};
use std::path::PathBuf;
use storage::EventBackfillOptions;
use storage::maintenance::backfill_events;

use crate::maintenance::USAGE;

#[derive(Debug, Clone)]
pub struct BackfillArgs {
    pub db: Option<PathBuf>,
    pub options: EventBackfillOptions,
}

/// # Errors
/// The usage text, for anything but `maintenance backfill-events` with its own
/// options.
pub fn parse_args(argv: impl Iterator<Item = String>) -> Result<BackfillArgs, String> {
    let mut argv = argv;
    if argv.next().as_deref() != Some("maintenance")
        || argv.next().as_deref() != Some("backfill-events")
    {
        return Err(USAGE.into());
    }
    let mut args = BackfillArgs {
        db: None,
        options: EventBackfillOptions::default(),
    };
    let mut seen = std::collections::BTreeSet::new();
    while let Some(arg) = argv.next() {
        if !seen.insert(arg.clone()) {
            return Err(USAGE.into());
        }
        match arg.as_str() {
            "--db" => args.db = Some(argv.next().ok_or(USAGE)?.into()),
            "--batch-rows" => {
                args.options.batch_rows = argv
                    .next()
                    .ok_or(USAGE)?
                    .parse()
                    .ok()
                    .filter(|n| (1..=100_000).contains(n))
                    .ok_or(USAGE)?;
            }
            "--max-rows" => {
                args.options.max_rows = Some(argv.next().ok_or(USAGE)?.parse().map_err(|_| USAGE)?);
            }
            _ => return Err(USAGE.into()),
        }
    }
    Ok(args)
}

/// # Errors
/// The storage error as text.
pub fn run(args: BackfillArgs) -> Result<Value, String> {
    let path = match args.db {
        Some(path) => path,
        None => config::user_config_dir()
            .ok_or("cannot resolve config directory; pass --db")?
            .join("evorch-events.db"),
    };
    backfill_events(&path, &args.options)
        .map(|summary| json!(summary))
        .map_err(|error| error.to_string())
}
