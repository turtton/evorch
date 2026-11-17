//! `evorch eval ...` (see [`crate::evaluation`]).
use std::process::ExitCode;

pub fn main(argv: Vec<String>) -> ExitCode {
    let command = match crate::evaluation::parse_args(argv.into_iter()) {
        Ok(command) => command,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(1);
        }
    };
    let runtime = match super::async_runtime() {
        Ok(runtime) => runtime,
        Err(code) => return code,
    };
    runtime.block_on(crate::evaluation::run(command))
}
