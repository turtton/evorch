//! Thread replay evaluation (ADR 0036 §3–§6): rebuild a recorded thread's start
//! in an isolated clone, replay it under a configuration with a simulated user,
//! and keep each run's database, diff, conversation and outcome.

pub mod conversation;
pub mod driver;
mod git;
pub mod output;
pub mod runner;
pub mod simulator;
pub mod source;
#[cfg(test)]
mod test_support;
pub mod workspace;

pub use runner::{
    EvalArgs, EvalError, Evaluation, FixedModels, Isolation, RunReport, RunnerDeps, run,
};
