//! `evorch reindex-threads`: the same durable request/lease/step path as the GUI,
//! run by the core.
//!
//! This module is the command line: it reads the arguments and builds the task.
//! [`crate::online_maintenance::reindex`] starts it in the core and follows it.
//! The core owns the database, so there is no `--db`; `--user-config` names the
//! configuration whose core runs it (ADR 0045 §10, stage 17d-3).

use std::path::PathBuf;

use workbench_protocol::namespaces::maintenance::ReindexThreads;

pub const USAGE: &str = "usage: evorch reindex-threads [--user-config <dir>] [--project <dir>] [--since <unix-ns>] [--include-evicted] [--wait]
--since is an inclusive, non-negative Unix nanosecond timestamp.
The index is the evorch core's, found in the user config directory (--user-config), so there is
no --db. The core runs the reindex; this command shows its progress on stderr.
Output is JSON: accepted, completed, completed_with_gaps, or failed.
accepted delegates a durable request to another owner; it does not mean completed.
Interrupting this command stops its reindex at the next batch; the request stays for the next owner.";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReindexArgs {
    pub user_config_dir: Option<PathBuf>,
    pub project: Option<PathBuf>,
    pub since_ns: Option<i64>,
    pub include_evicted: bool,
    pub wait: bool,
}

/// Parses everything after `evorch`, starting with `reindex-threads`.
///
/// # Errors
/// The text to print: bad arguments, or `--db`, which this command no longer has.
pub fn parse_args(mut argv: impl Iterator<Item = String>) -> Result<ReindexArgs, String> {
    if argv.next().as_deref() != Some("reindex-threads") {
        return Err(USAGE.into());
    }
    let mut args = ReindexArgs::default();
    let mut seen = std::collections::BTreeSet::new();
    while let Some(arg) = argv.next() {
        if arg == "--db" {
            return Err(format!(
                "--db is not accepted: the evorch core reads its own database from the user config \
                 directory (--user-config); drop --db\n{USAGE}"
            ));
        }
        if !seen.insert(arg.clone()) {
            return Err(format!("duplicate argument: {arg}\n{USAGE}"));
        }
        match arg.as_str() {
            "--wait" => args.wait = true,
            "--include-evicted" => args.include_evicted = true,
            "--user-config" | "--project" | "--since" => {
                let value = argv
                    .next()
                    .filter(|value| !value.is_empty() && !value.starts_with("--"))
                    .ok_or(USAGE)?;
                match arg.as_str() {
                    "--user-config" => args.user_config_dir = Some(PathBuf::from(value)),
                    "--project" => args.project = Some(PathBuf::from(value)),
                    _ => {
                        args.since_ns = Some(
                            value
                                .parse::<i64>()
                                .ok()
                                .filter(|value| *value >= 0)
                                .ok_or_else(|| format!("invalid --since\n{USAGE}"))?,
                        );
                    }
                }
            }
            _ => return Err(USAGE.into()),
        }
    }
    Ok(args)
}

/// The task the arguments ask the core for. The project is made canonical here,
/// where the path the user typed is relative to: the core only sees the result.
///
/// # Errors
/// The text of an invalid project (`invalid_arguments`).
pub fn task(args: &ReindexArgs) -> Result<ReindexThreads, String> {
    let canonical_project = args
        .project
        .as_ref()
        .map(|path| {
            let path = path.canonicalize().map_err(|error| error.to_string())?;
            if !path.is_dir() {
                return Err("--project must be a directory".to_owned());
            }
            path.into_os_string()
                .into_string()
                .map_err(|_| "--project must be UTF-8".to_owned())
        })
        .transpose()?;
    Ok(ReindexThreads {
        canonical_project,
        since_ns: args.since_ns,
        include_evicted: args.include_evicted,
        wait: args.wait,
    })
}

#[cfg(test)]
mod tests;
