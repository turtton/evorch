//! Thread replay evaluation (ADR 0036 §3–§10): rebuild a recorded thread's start
//! in an isolated clone, replay it under a configuration with a simulated user,
//! keep each run's database, diff, conversation and outcome, then reprice,
//! judge and report the runs.

pub mod conversation;
pub mod cost;
pub mod driver;
pub mod evaluator;
mod git;
pub mod judge;
pub mod metrics;
pub mod output;
pub mod report;
pub mod roles;
pub mod runner;
pub mod simulator;
pub mod source;
#[cfg(test)]
mod test_support;
pub mod verify;
pub mod workspace;

pub use report::{Report, ReportArgs, ReportDeps, cross_thread_table, report};
pub use runner::{
    EvalArgs, EvalError, Evaluation, FixedModels, Isolation, RunReport, RunnerDeps, run,
};
