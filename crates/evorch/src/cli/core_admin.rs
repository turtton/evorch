//! `evorch core restart …`: manage the running core's safe restart.
//!
//! The management CLI is a client of a core that is already running. It never
//! starts one (no core means exit 1), never opens the database and has no
//! runtime of its own beyond the one that drives its requests.
//!
//! * `restart targets` / `restart status` – read.
//! * `restart request [--target ID] [--yes] [--no-wait]` – ask for a restart.
//!   The target defaults to `current`. Without `--yes` a terminal is asked;
//!   without either nothing is sent.
//! * `restart cancel` – turn a restart that is not yet captured back.
//! * `restart interrupt (--blocker N | --run ID) [--yes]` – interrupt one
//!   shell that holds the restart up, after confirming exactly it.
//!
//! * `quarantine list|show ID|discard ID [--yes]` and `graphs list|show ID|release ID
//!   [--yes]|resume ID [--yes]` – recover what a restart left behind (Q1/Q2). A
//!   change is bound to the detail the core just gave, needs a confirmation and
//!   goes out once. A refused `restart request` names the undisposed graphs.
//!
//! Exit codes: 0 done, 1 failure (including "no core is running"), 130 the
//! person pressed Ctrl-C: only the wait ended; nothing was cancelled or stopped.
//!
//! [`main`] is wired to the `core` route; [`run`] is what the end-to-end tests
//! call.
mod args;
#[cfg(target_os = "linux")]
mod flow;
#[cfg(target_os = "linux")]
mod interrupt;

pub use args::{AdminCommand, GraphsOp, ParseError, QuarantineOp, parse};
#[cfg(target_os = "linux")]
pub use flow::{AdminEnv, run};

use std::process::ExitCode;

/// Run the command line (`core restart …`, with or without the leading `core`).
#[cfg(target_os = "linux")]
pub fn main(argv: Vec<String>) -> ExitCode {
    use super::core_client::exit;
    let parsed = match parse(&argv) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("{error}");
            return exit::code(exit::FAILURE);
        }
    };
    let runtime = match super::async_runtime() {
        Ok(runtime) => runtime,
        Err(code) => return code,
    };
    let code = runtime.block_on(interrupt::run_process(parsed));
    exit::code(code)
}

#[cfg(not(target_os = "linux"))]
pub fn main(argv: Vec<String>) -> ExitCode {
    let _ = argv;
    eprintln!("safe restart management is only available on Linux");
    ExitCode::from(1)
}
