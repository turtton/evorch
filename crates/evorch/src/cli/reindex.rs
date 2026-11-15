//! `evorch reindex-threads ...` (see [`crate::reindex_threads`]).
use std::process::ExitCode;

use crate::reindex_threads::ReindexOutcome;
use storage::thread_index::{ReindexResult, RequestFailure};

pub fn main(argv: Vec<String>) -> ExitCode {
    let result = match crate::reindex_threads::parse_args(argv.into_iter()) {
        Ok(args) => match crate::reindex_threads::run(args) {
            Ok(ReindexOutcome::Result(result)) => result,
            Ok(ReindexOutcome::Cancelled { request_id }) => {
                eprintln!("request {request_id} cancelled; not failed");
                return ExitCode::from(1);
            }
            Err(error) => {
                eprintln!("{}", error.message);
                ReindexResult::Failed {
                    request_id: None,
                    reason: error.reason,
                }
            }
        },
        Err(error) => {
            eprintln!("{error}");
            ReindexResult::Failed {
                request_id: None,
                reason: RequestFailure::InvalidArguments,
            }
        }
    };
    match serde_json::to_string_pretty(&result) {
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
