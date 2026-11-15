//! `evorch maintenance backfill-events` (online; see [`crate::backfill`]).
use std::process::ExitCode;

pub fn main(argv: Vec<String>) -> ExitCode {
    let result = crate::backfill::parse_args(argv.into_iter()).and_then(crate::backfill::run);
    super::print_json(result)
}
