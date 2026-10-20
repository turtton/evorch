//! `evorch eval run`: replay a recorded thread under the user's configuration
//! and a candidate overlay, keeping every replay's results (ADR 0036).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use eval::{EvalArgs, Isolation, RunReport, RunnerDeps};
use routing::EnvLookup;

pub const USAGE: &str = "usage: evorch eval run --thread <thread-id> [--candidate <overlay.toml>] [--repeat <n>] [--output <dir>] [--db <path>] [--user-config <dir>]
Replays the thread <n> times (default eval.repeat_target) under the user's configuration,
then <n> times with the candidate overlay merged over it. The overlay is a config file
fragment; when a role_profile is active, change role_profiles.<name> rather than agents or
routing. It cannot change eval, tools or comment_checker.
The default database is <user-config-dir>/evorch-events.db, the GUI's store. The default
output is <user-config-dir>/eval/<UTC YYYYmmdd-HHMMSS>-<thread-id>; it must be missing or empty.";

/// Parses everything after `evorch`, starting with `eval run`.
pub fn parse_args(argv: impl Iterator<Item = String>) -> Result<EvalArgs, String> {
    let mut argv = argv;
    if argv.next().as_deref() != Some("eval") || argv.next().as_deref() != Some("run") {
        return Err(USAGE.into());
    }
    let mut args = EvalArgs::default();
    let mut thread = None;
    while let Some(flag) = argv.next() {
        let value = argv.next().ok_or_else(|| USAGE.to_owned())?;
        match flag.as_str() {
            "--thread" => thread = Some(value),
            "--candidate" => args.candidate = Some(PathBuf::from(value)),
            "--repeat" => {
                args.repeat = Some(
                    value
                        .parse()
                        .ok()
                        .filter(|repeat| *repeat > 0)
                        .ok_or_else(|| format!("--repeat must be a positive integer\n{USAGE}"))?,
                );
            }
            "--output" => args.output = Some(PathBuf::from(value)),
            "--db" => args.db = Some(PathBuf::from(value)),
            "--user-config" => args.user_config_dir = Some(PathBuf::from(value)),
            _ => return Err(USAGE.into()),
        }
    }
    args.thread_id = thread
        .filter(|thread| !thread.is_empty())
        .ok_or_else(|| USAGE.to_owned())?;
    Ok(args)
}

/// Runs the evaluation, printing one line per replay as it ends.
pub async fn run(args: EvalArgs, env: Arc<dyn EnvLookup>) -> Result<String, String> {
    let catalog = match catalog::ModelCatalog::load_or_refresh(&catalog::default_cache_dir()).await
    {
        Ok(catalog) => Some(Arc::new(catalog)),
        Err(error) => {
            eprintln!("model catalog unavailable; usage is priced from config only: {error}");
            None
        }
    };
    let deps = RunnerDeps {
        env,
        isolation: Isolation::Production,
        models: None,
        credentials: None,
        catalog,
    };
    let evaluation = eval::run(args, deps, |report| println!("{}", summary(report)))
        .await
        .map_err(|error| error.to_string())?;
    Ok(format!("Results in {}", evaluation.dir.display()))
}

fn summary(report: &RunReport) -> String {
    let outcome = &report.outcome;
    let status = serde_json::to_value(outcome.status)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default();
    format!(
        "{}-{}: {status} ({}); {} user turns, {} tokens, {} tool calls, {:.1} s, {} denied, {} pushed refs",
        report.label,
        report.index,
        outcome.reason,
        outcome.user_turns,
        outcome.tokens,
        outcome.tool_calls,
        Duration::from_millis(outcome.elapsed_ms).as_secs_f64(),
        outcome.denied_requests.len(),
        outcome.pushed_refs.len(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<EvalArgs, String> {
        parse_args(args.iter().map(|arg| (*arg).to_owned()))
    }

    #[test]
    fn every_flag_is_parsed() {
        // Given: a full `eval run` command line.
        let argv = [
            "eval",
            "run",
            "--thread",
            "thread-1",
            "--candidate",
            "c.toml",
            "--repeat",
            "3",
            "--output",
            "out",
            "--db",
            "events.db",
            "--user-config",
            "config",
        ];

        // When: it is parsed.
        let args = parse(&argv).unwrap();

        // Then: every value lands in its field.
        assert_eq!(
            args,
            EvalArgs {
                thread_id: "thread-1".into(),
                candidate: Some("c.toml".into()),
                repeat: Some(3),
                output: Some("out".into()),
                db: Some("events.db".into()),
                user_config_dir: Some("config".into()),
            }
        );
    }

    #[test]
    fn only_the_thread_is_required() {
        let args = parse(&["eval", "run", "--thread", "t"]).unwrap();
        assert_eq!(
            args,
            EvalArgs {
                thread_id: "t".into(),
                ..EvalArgs::default()
            }
        );
    }

    #[test]
    fn malformed_command_lines_are_refused_with_usage() {
        for argv in [
            &["eval"][..],
            &["eval", "report", "--thread", "t"],
            &["eval", "run"],
            &["eval", "run", "--thread"],
            &["eval", "run", "--thread", ""],
            &["eval", "run", "--thread", "t", "--unknown", "x"],
            &["eval", "run", "--thread", "t", "--repeat", "0"],
            &["eval", "run", "--thread", "t", "--repeat", "many"],
        ] {
            let error = parse(argv).unwrap_err();
            assert!(
                error.contains("usage: evorch eval run"),
                "{argv:?}: {error}"
            );
        }
    }
}
