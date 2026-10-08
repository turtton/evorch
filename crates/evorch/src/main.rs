//! evorch コマンドのバイナリエントリポイントです。

use std::process::ExitCode;
use std::sync::Arc;

use event_bus::AgentRunPhase;
use evorch::headless::{self, SandboxChoice};
use routing::ProcessEnv;

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    // Panicking runs then report where they panicked, not only the payload.
    runtime::panic_capture::install();

    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().is_some_and(|command| command == "benchmark") {
        return benchmark(argv);
    }
    if argv.first().is_some_and(|command| command == "inspect") {
        return inspect(argv);
    }

    let args = match headless::parse_args(argv.into_iter()) {
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
            if let Some(text) = outcome.final_text {
                eprintln!("{text}");
            }
            ExitCode::from(1)
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(1)
        }
    }
}

fn benchmark(argv: Vec<String>) -> ExitCode {
    let args = match evorch::benchmark::parse_args(argv.into_iter()) {
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
    match runtime.block_on(evorch::benchmark::run(
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

fn inspect(argv: Vec<String>) -> ExitCode {
    let result = evorch::inspect::parse_args(argv.into_iter()).and_then(evorch::inspect::run);
    match result
        .and_then(|value| serde_json::to_string_pretty(&value).map_err(|error| error.to_string()))
    {
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
