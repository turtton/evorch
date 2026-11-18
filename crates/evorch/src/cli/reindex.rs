//! `evorch reindex-threads ...`: a task of the core (see
//! [`crate::reindex_threads`] and [`crate::online_maintenance`]).
//!
//! stdout is the JSON result the offline command printed (`accepted`,
//! `completed`, `completed_with_gaps`, `failed`); `failed` ends with exit code 1
//! after the print. What leaves no result (this reindex stopped, or cancelled by
//! another owner) is said on stderr with exit code 1. A reindex that could not
//! be run at all (no core, a lost connection, a refusal) prints a `failed`
//! result as well, so a script reading stdout sees one shape.
use std::process::ExitCode;

use workbench_protocol::namespaces::maintenance::{ReindexFailure, ReindexResult};

use super::core_client::CliError;
use crate::online_maintenance::{self, ReindexEnd};

fn failed(reason: ReindexFailure) -> ReindexResult {
    ReindexResult::Failed {
        request_id: None,
        reason,
    }
}

/// What the command prints to stdout, and whether it succeeded.
fn print(result: &ReindexResult) -> ExitCode {
    match serde_json::to_string_pretty(result) {
        Ok(json) => println!("{json}"),
        Err(error) => {
            eprintln!("failed to serialize reindex result: {error}");
            return ExitCode::from(1);
        }
    }
    if matches!(result, ReindexResult::Failed { .. }) {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

pub fn main(argv: Vec<String>) -> ExitCode {
    let args = match crate::reindex_threads::parse_args(argv.into_iter()) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("{error}");
            return print(&failed(ReindexFailure::InvalidArguments));
        }
    };
    let task = match crate::reindex_threads::task(&args) {
        Ok(task) => task,
        Err(error) => {
            eprintln!("{error}");
            return print(&failed(ReindexFailure::InvalidArguments));
        }
    };
    let runtime = match super::async_runtime() {
        Ok(runtime) => runtime,
        Err(code) => return code,
    };
    runtime.block_on(async {
        finish(online_maintenance::execute_reindex(args.user_config_dir, &task).await)
    })
}

/// Print how a reindex ended and say whether it succeeded. A reindex that could
/// not be run at all prints a `failed` result as well as its reason on stderr.
pub fn finish(end: Result<ReindexEnd, CliError>) -> ExitCode {
    match end {
        Ok(ReindexEnd::Result(result)) => print(&result),
        Ok(ReindexEnd::Stopped(message)) => {
            eprintln!("{message}");
            ExitCode::from(1)
        }
        Err(error) => {
            eprintln!("{error}");
            print(&failed(online_maintenance::failure_of(&error)))
        }
    }
}
