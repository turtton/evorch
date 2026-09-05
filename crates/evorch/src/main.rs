//! evorch コマンドのバイナリエントリポイントです。

use std::process::ExitCode;
use std::sync::Arc;

use event_bus::AgentRunPhase;
use evorch::headless::{self, SandboxChoice};
use routing::ProcessEnv;

fn main() -> ExitCode {
    let args = match headless::parse_args(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(1);
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("failed to start async runtime: {error}");
            return ExitCode::from(1);
        }
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
            ExitCode::from(1)
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(1)
        }
    }
}
