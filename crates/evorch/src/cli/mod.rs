//! Where a command line goes. [`route`] names the command and [`dispatch`] hands
//! the arguments to it; each command has a module of its own here, so the PRs
//! that move one to the core (run, eval, reindex, backfill) or keep it as a tool
//! (offline maintenance, inspect, benchmark) edit only their module. Moving a
//! command between modules, or adding one, is the integration PR's change.
//!
//! The `js-worker` subprocess is not routed here: `main` starts it before
//! logging and panic capture are set up.
use std::process::ExitCode;

pub mod backfill;
pub mod benchmark;
pub mod eval;
pub mod inspect;
pub mod maintenance;
pub mod reindex;
pub mod run;

/// The command a command line asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// `evorch [run options]`: one agent run.
    Run,
    Eval,
    Reindex,
    /// `evorch maintenance backfill-events`: the online backfill.
    Backfill,
    /// `evorch maintenance <anything else>`: the offline tool.
    Maintenance,
    Inspect,
    Benchmark,
}

/// The command `argv` (without the program name) names.
pub fn route(argv: &[String]) -> Route {
    match argv.first().map(String::as_str) {
        Some("benchmark") => Route::Benchmark,
        Some("maintenance") if argv.get(1).map(String::as_str) == Some("backfill-events") => {
            Route::Backfill
        }
        Some("maintenance") => Route::Maintenance,
        Some("inspect") => Route::Inspect,
        Some("eval") => Route::Eval,
        Some("reindex-threads") => Route::Reindex,
        _ => Route::Run,
    }
}

/// Run the command `argv` names and return its exit code.
pub fn dispatch(argv: Vec<String>) -> ExitCode {
    match route(&argv) {
        Route::Run => run::main(argv),
        Route::Eval => eval::main(argv),
        Route::Reindex => reindex::main(argv),
        Route::Backfill => backfill::main(argv),
        Route::Maintenance => maintenance::main(argv),
        Route::Inspect => inspect::main(argv),
        Route::Benchmark => benchmark::main(argv),
    }
}

/// Print `result` as pretty JSON and succeed, or print the error and exit 1.
fn print_json(result: Result<serde_json::Value, String>) -> ExitCode {
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

/// A multi-thread async runtime, or the exit code that reports why there is none.
fn async_runtime() -> Result<tokio::runtime::Runtime, ExitCode> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| {
            eprintln!("failed to start async runtime: {error}");
            ExitCode::from(1)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route_of(args: &[&str]) -> Route {
        route(&args.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>())
    }

    #[test]
    fn each_command_line_goes_to_its_module() {
        assert_eq!(route_of(&[]), Route::Run);
        assert_eq!(route_of(&["--prompt", "x"]), Route::Run);
        assert_eq!(route_of(&["eval", "run"]), Route::Eval);
        assert_eq!(route_of(&["reindex-threads"]), Route::Reindex);
        assert_eq!(route_of(&["inspect", "--db", "x"]), Route::Inspect);
        assert_eq!(route_of(&["benchmark"]), Route::Benchmark);
        assert_eq!(route_of(&["maintenance", "vacuum"]), Route::Maintenance);
        assert_eq!(route_of(&["maintenance"]), Route::Maintenance);
        assert_eq!(
            route_of(&["maintenance", "backfill-events", "--max-rows", "1"]),
            Route::Backfill
        );
    }
}
