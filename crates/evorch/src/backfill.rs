//! `evorch maintenance backfill-events`: converts the legacy subkind/run_id
//! columns of the events table in bounded batches, next to live writers.
//!
//! It is a task of the core (ADR 0045 §10, stage 17d): this module is the
//! command line, [`crate::online_maintenance`] starts and follows the task. The
//! core owns the database, so there is no `--db`; `--user-config` names the
//! configuration whose core runs it. The offline commands are in
//! [`crate::maintenance`].

use std::path::PathBuf;

use workbench_protocol::namespaces::maintenance::MAX_BATCH_ROWS;

pub use crate::online_maintenance::BackfillRequest;

pub const USAGE: &str = "usage: evorch maintenance backfill-events [--user-config <dir>] [--batch-rows <1..100000>] [--max-rows <N>]
Converts the legacy subkind/run_id columns of the events table in the evorch core, which is started
when none is running; it does not work without it, and the core reads its own database from the
user config directory (--user-config), so there is no --db. It is safe next to live writers: the
work is done in bounded batches (--batch-rows, default 1000), and at most --max-rows rows are
handled in all (default: every row). Progress goes to stderr, one line per batch; the result goes
to stdout as JSON {converted, undecodable, batches, reattributed, remaining}. Only one backfill
runs at a time; a second one fails as busy. Interrupting or killing this command, or losing its
connection, stops the backfill it started (and no other work): the core ends it at its next batch
boundary, and what was converted stays converted. Run it again to continue.";

/// # Errors
/// The usage text, for anything but `maintenance backfill-events` with its own
/// options.
pub fn parse_args(argv: impl Iterator<Item = String>) -> Result<BackfillRequest, String> {
    let mut argv = argv;
    if argv.next().as_deref() != Some("maintenance")
        || argv.next().as_deref() != Some("backfill-events")
    {
        return Err(USAGE.into());
    }
    let mut request = BackfillRequest::default();
    let mut seen = std::collections::BTreeSet::new();
    while let Some(arg) = argv.next() {
        if arg == "--db" {
            return Err(format!(
                "--db is not accepted: the evorch core reads its own database from the user config \
                 directory (--user-config); drop --db\n{USAGE}"
            ));
        }
        if !seen.insert(arg.clone()) {
            return Err(USAGE.into());
        }
        match arg.as_str() {
            "--user-config" => {
                request.user_config_dir = Some(PathBuf::from(argv.next().ok_or(USAGE)?));
            }
            "--batch-rows" => {
                request.batch_rows = Some(
                    argv.next()
                        .ok_or(USAGE)?
                        .parse()
                        .ok()
                        .filter(|rows| (1..=MAX_BATCH_ROWS).contains(rows))
                        .ok_or(USAGE)?,
                );
            }
            "--max-rows" => {
                request.max_rows = Some(
                    argv.next()
                        .ok_or(USAGE)?
                        .parse()
                        .ok()
                        .filter(|rows| *rows >= 1)
                        .ok_or(USAGE)?,
                );
            }
            _ => return Err(USAGE.into()),
        }
    }
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<BackfillRequest, String> {
        parse_args(
            ["maintenance", "backfill-events"]
                .into_iter()
                .chain(args.iter().copied())
                .map(str::to_owned),
        )
    }

    #[test]
    fn every_flag_is_parsed() {
        // Given / When: a full command line.
        let request = parse(&[
            "--user-config",
            "config",
            "--batch-rows",
            "50",
            "--max-rows",
            "7",
        ])
        .unwrap();

        // Then: every value lands in its field.
        assert_eq!(
            request,
            BackfillRequest {
                user_config_dir: Some("config".into()),
                batch_rows: Some(50),
                max_rows: Some(7),
            }
        );
        assert_eq!(parse(&[]).unwrap(), BackfillRequest::default());
    }

    #[test]
    fn db_is_refused_with_the_reason() {
        let error = parse(&["--db", "events.db"]).unwrap_err();
        assert!(error.contains("--db is not accepted"), "{error}");
    }

    #[test]
    fn bad_options_give_the_usage() {
        for args in [
            vec!["--batch-rows", "0"],
            vec!["--batch-rows", "100001"],
            vec!["--max-rows", "0"],
            vec!["--max-rows", "x"],
            vec!["--max-rows"],
            vec!["--user-config"],
            vec!["--user-config", "a", "--user-config", "b"],
            vec!["--apply"],
        ] {
            assert_eq!(parse(&args).unwrap_err(), USAGE, "{args:?}");
        }
        assert_eq!(
            parse_args(["maintenance", "vacuum"].into_iter().map(str::to_owned)).unwrap_err(),
            USAGE
        );
    }
}
