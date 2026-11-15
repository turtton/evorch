//! `evorch benchmark ...`: the development tool with storage of its own (see
//! [`crate::benchmark`]).
use std::process::ExitCode;
use std::sync::Arc;

use routing::ProcessEnv;

use crate::headless::SandboxChoice;

pub fn main(argv: Vec<String>) -> ExitCode {
    let args = match crate::benchmark::parse_args(argv.into_iter()) {
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
    match runtime.block_on(crate::benchmark::run(
        args,
        Arc::new(ProcessEnv),
        SandboxChoice::Production,
    )) {
        Ok(result) => {
            println!("{result}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(1)
        }
    }
}
