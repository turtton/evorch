//! `evorch inspect ...`: read-only diagnostics (see [`crate::inspect`]).
use std::process::ExitCode;

pub fn main(argv: Vec<String>) -> ExitCode {
    let result = crate::inspect::parse_args(argv.into_iter()).and_then(crate::inspect::run);
    let failed = result.as_ref().is_ok_and(crate::inspect::reports_failure);
    match result
        .and_then(|value| serde_json::to_string_pretty(&value).map_err(|error| error.to_string()))
    {
        Ok(text) => {
            println!("{text}");
            if failed {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(1)
        }
    }
}
