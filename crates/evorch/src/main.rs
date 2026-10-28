//! evorch コマンドのバイナリエントリポイントです。

use std::process::ExitCode;
use std::sync::Arc;

use event_bus::AgentRunPhase;
use evorch::headless::{self, SandboxChoice};
use routing::ProcessEnv;

fn main() -> ExitCode {
    // The sandboxed run_js worker (ADR 0039) must not initialise logging or panic capture.
    if std::env::args().nth(1).as_deref() == Some("js-worker") {
        return evorch::js_worker::main();
    }
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
    if argv.first().is_some_and(|command| command == "maintenance") {
        return maintenance(argv);
    }
    if argv.first().is_some_and(|command| command == "inspect") {
        return inspect(argv);
    }
    if argv.first().is_some_and(|command| command == "eval") {
        return evaluate(argv);
    }
    if argv
        .first()
        .is_some_and(|command| command == "reindex-threads")
    {
        return reindex_threads(argv);
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

fn reindex_threads(argv: Vec<String>) -> ExitCode {
    use evorch::reindex_threads::ReindexOutcome;
    use storage::thread_index::{ReindexResult, RequestFailure};
    let result = match evorch::reindex_threads::parse_args(argv.into_iter()) {
        Ok(args) => match evorch::reindex_threads::run(args) {
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

fn evaluate(argv: Vec<String>) -> ExitCode {
    let args = match evorch::evaluation::parse_args(argv.into_iter()) {
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
    match runtime.block_on(evorch::evaluation::run(args, Arc::new(ProcessEnv))) {
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

fn maintenance(argv: Vec<String>) -> ExitCode {
    let result =
        evorch::maintenance::parse_args(argv.into_iter()).and_then(evorch::maintenance::run);
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
