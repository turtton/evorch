//! `evorch maintenance backfill-events`: a task of the core (see
//! [`crate::backfill`] and [`crate::online_maintenance`]).
use std::process::ExitCode;

use super::core_client;

pub fn main(argv: Vec<String>) -> ExitCode {
    let request = match crate::backfill::parse_args(argv.into_iter()) {
        Ok(request) => request,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(1);
        }
    };
    let runtime = match super::async_runtime() {
        Ok(runtime) => runtime,
        Err(code) => return code,
    };
    runtime
        .block_on(async { core_client::finish(crate::online_maintenance::execute(request).await) })
}
