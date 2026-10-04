//! Independent frozen-delegate benchmarks using the production runtime.

mod engine;
mod record_run;
mod replay_run;
mod report;
pub mod spec;
mod storage;
mod verifier;

pub use record_run::record;
pub use replay_run::replay;
pub use report::{BenchmarkReport, read_report};
pub use spec::{BenchmarkArgs, BenchmarkCommand, Budget, TaskSpec, Verifier, parse_args};
pub use storage::{Baseline, Evaluation, Trial};

pub type BenchmarkResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub(crate) fn invalid(message: impl Into<String>) -> Box<dyn std::error::Error + Send + Sync> {
    std::io::Error::other(message.into()).into()
}

/// Execute a benchmark command. Production is the public CLI default; the
/// unchecked seam exists for localhost mock-provider tests only.
pub async fn run(
    args: BenchmarkArgs,
    env: std::sync::Arc<dyn routing::EnvLookup>,
    sandbox: crate::headless::SandboxChoice,
) -> BenchmarkResult<String> {
    match args.command {
        BenchmarkCommand::Record { spec, output } => {
            let baseline = record(&spec, &output, args.user_config_dir, env, sandbox).await?;
            if baseline.poisoned.is_some() || baseline.checkpoint.is_none() {
                return Err(invalid(format!(
                    "baseline cannot be replayed; evidence retained at {}: {}",
                    output.display(),
                    baseline
                        .poisoned
                        .as_deref()
                        .or(baseline.full.error.as_deref())
                        .unwrap_or("no checkpoint captured")
                )));
            }
            Ok(format!(
                "Recorded benchmark at {}; trusted verifier: {}",
                output.display(),
                if baseline.full.evaluation.passed {
                    "pass"
                } else {
                    "fail"
                }
            ))
        }
        BenchmarkCommand::Replay {
            record,
            trial,
            candidate,
        } => {
            let outcome = replay(
                &record,
                &trial,
                candidate,
                args.user_config_dir,
                env,
                sandbox,
            )
            .await?;
            if outcome.phase != event_bus::AgentRunPhase::Done {
                return Err(invalid(format!(
                    "trial {trial} did not complete; evidence retained at {}: {}",
                    record.display(),
                    outcome.error.as_deref().unwrap_or("execution failed")
                )));
            }
            Ok(format!(
                "Recorded trial {trial} at {}; trusted verifier: {}",
                record.display(),
                if outcome.evaluation.passed {
                    "pass"
                } else {
                    "fail"
                }
            ))
        }
        BenchmarkCommand::Report { record } => Ok(read_report(&record)?.markdown()),
    }
}
