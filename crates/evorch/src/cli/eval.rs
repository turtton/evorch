//! `evorch eval ...` (see [`crate::evaluation`]).
use std::process::ExitCode;
use std::sync::Arc;

use routing::ProcessEnv;

pub fn main(argv: Vec<String>) -> ExitCode {
    let args = match crate::evaluation::parse_args(argv.into_iter()) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(1);
        }
    };
    let runtime = match super::async_runtime() {
        Ok(runtime) => runtime,
        Err(code) => return code,
    };
    match runtime.block_on(crate::evaluation::run(args, Arc::new(ProcessEnv))) {
        Ok(text) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(1)
        }
    }
}
