//! `evorch maintenance <command>`: the offline tool (see [`crate::maintenance`]).
use std::process::ExitCode;

pub fn main(argv: Vec<String>) -> ExitCode {
    let result = crate::maintenance::parse_args(argv.into_iter()).and_then(crate::maintenance::run);
    super::print_json(result)
}
