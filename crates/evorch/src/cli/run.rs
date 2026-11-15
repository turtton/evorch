//! `evorch [run options]`: one agent run (headless; see [`crate::headless`]).
use std::process::ExitCode;
use std::sync::Arc;

use event_bus::AgentRunPhase;
use routing::ProcessEnv;

use crate::headless::{self, SandboxChoice};

pub fn main(argv: Vec<String>) -> ExitCode {
    let args = match headless::parse_args(argv.into_iter()) {
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
    match runtime.block_on(headless::run_headless(
        args,
        Arc::new(ProcessEnv),
        SandboxChoice::Production,
    )) {
        Ok(outcome) if outcome.phase == AgentRunPhase::Done => {
            if let Some(text) = outcome.final_text {
                println!("{text}");
            }
            ExitCode::SUCCESS
        }
        Ok(outcome) => {
            eprintln!("run ended in phase {:?}", outcome.phase);
            if let Some(text) = outcome.final_text {
                eprintln!("{text}");
            }
            ExitCode::from(1)
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(headless::exit_code(&error))
        }
    }
}
