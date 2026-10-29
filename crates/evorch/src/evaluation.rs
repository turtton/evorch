//! `evorch eval run` and `evorch eval report`: replay a recorded thread under
//! the user's configuration and a candidate overlay, then judge, reprice and
//! report the replays (ADR 0036).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use eval::{EvalArgs, Isolation, ReportArgs, ReportDeps, RunReport, RunnerDeps};
use routing::EnvLookup;

pub const USAGE: &str = "usage: evorch eval run --thread <thread-id> [--candidate <overlay.toml>] [--repeat <n>] [--output <dir>] [--db <path>] [--user-config <dir>] [--problem <text>] [--verify <command>] [--verify-timeout <secs>] [--report|--no-report]
       evorch eval report --output <dir> [--output <dir> ...] [--user-config <dir>] [--rejudge]
`eval run` replays the thread <n> times (default eval.repeat_target) under the user's
configuration, then <n> times with the candidate overlay merged over it. The overlay is a
config file fragment; when a role_profile is active, change role_profiles.<name> rather than
agents or routing. It cannot change eval, tools or comment_checker. --problem describes what
went wrong in the thread and is judged as its own aspect. --verify runs a command in each
replay's final workspace, without network, failing after --verify-timeout (default 600) s.
The default database is <user-config-dir>/evorch-events.db, the GUI's store. The default
output is <user-config-dir>/eval/<UTC YYYYmmdd-HHMMSS>-<thread-id>; it must be missing or
empty. Unless --no-report is given, the directory is then reported as by `eval report`.
`eval report` reprices every run from one price table, judges each baseline/candidate pair
in both orders (cached in <dir>/judgments; --rejudge asks again), and writes
<dir>/report.json and <dir>/report.md. Several directories are also tabulated side by side.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Run { args: EvalArgs, report: bool },
    Report(ReportArgs),
}

/// Parses everything after `evorch`, starting with `eval run` or `eval report`.
pub fn parse_args(argv: impl Iterator<Item = String>) -> Result<Command, String> {
    let mut argv = argv;
    if argv.next().as_deref() != Some("eval") {
        return Err(USAGE.into());
    }
    match argv.next().as_deref() {
        Some("run") => parse_run(argv),
        Some("report") => parse_report(argv),
        _ => Err(USAGE.into()),
    }
}

fn parse_run(mut argv: impl Iterator<Item = String>) -> Result<Command, String> {
    let mut args = EvalArgs::default();
    let mut thread = None;
    let mut report = true;
    while let Some(flag) = argv.next() {
        if matches!(flag.as_str(), "--report" | "--no-report") {
            report = flag == "--report";
            continue;
        }
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
            "--problem" => args.problem = Some(value).filter(|text| !text.trim().is_empty()),
            "--verify" => args.verify = Some(value).filter(|text| !text.trim().is_empty()),
            "--verify-timeout" => {
                args.verify_timeout = Some(Duration::from_secs(
                    value.parse().ok().filter(|secs| *secs > 0).ok_or_else(|| {
                        format!("--verify-timeout must be a positive number of seconds\n{USAGE}")
                    })?,
                ));
            }
            _ => return Err(USAGE.into()),
        }
    }
    args.thread_id = thread
        .filter(|thread| !thread.is_empty())
        .ok_or_else(|| USAGE.to_owned())?;
    Ok(Command::Run { args, report })
}

fn parse_report(mut argv: impl Iterator<Item = String>) -> Result<Command, String> {
    let mut args = ReportArgs::default();
    while let Some(flag) = argv.next() {
        if flag == "--rejudge" {
            args.rejudge = true;
            continue;
        }
        let value = argv.next().ok_or_else(|| USAGE.to_owned())?;
        match flag.as_str() {
            "--output" => args.dirs.push(PathBuf::from(value)),
            "--user-config" => args.user_config_dir = Some(PathBuf::from(value)),
            _ => return Err(USAGE.into()),
        }
    }
    if args.dirs.is_empty() {
        return Err(USAGE.into());
    }
    Ok(Command::Report(args))
}

/// Runs the command, printing one line per replay as it ends.
pub async fn run(command: Command, env: Arc<dyn EnvLookup>) -> Result<String, String> {
    let catalog = match catalog::ModelCatalog::load_or_refresh(&catalog::default_cache_dir()).await
    {
        Ok(catalog) => Some(Arc::new(catalog)),
        Err(error) => {
            eprintln!("model catalog unavailable; usage is priced from config only: {error}");
            None
        }
    };
    let report_deps = |env| ReportDeps {
        env,
        judge: None,
        credentials: None,
        catalog: catalog.clone(),
    };
    match command {
        Command::Run { args, report } => {
            let user_config_dir = args.user_config_dir.clone();
            let deps = RunnerDeps {
                env: Arc::clone(&env),
                isolation: Isolation::Production,
                models: None,
                credentials: None,
                catalog: catalog.clone(),
            };
            let result = eval::run(args, deps, |report| println!("{}", summary(report))).await;
            let (dir, failure) = match result {
                Ok(evaluation) => (evaluation.dir, None),
                Err(error) => {
                    // A failed preflight still owns a complete manifest and
                    // evidence. Report them, but never turn failure into success.
                    let eval::EvalError::Preflight { report: path, .. } = &error else {
                        return Err(error.to_string());
                    };
                    if !report {
                        return Err(error.to_string());
                    }
                    let dir = path.parent().ok_or_else(|| error.to_string())?.to_owned();
                    (dir, Some(error.to_string()))
                }
            };
            let results = failure
                .clone()
                .unwrap_or_else(|| format!("Results in {}", dir.display()));
            if !report {
                return Ok(results);
            }
            let args = ReportArgs {
                dirs: vec![dir.clone()],
                user_config_dir,
                rejudge: false,
            };
            eval::report(args, &report_deps(env))
                .await
                .map_err(|error| format!("{results}; the report failed: {error}"))?;
            let message = format!(
                "{results}\nReport in {}",
                dir.join(eval::output::REPORT_MARKDOWN_FILE).display()
            );
            if failure.is_some() {
                Err(message)
            } else {
                Ok(message)
            }
        }
        Command::Report(args) => {
            let dirs = args.dirs.clone();
            let reports = eval::report(args, &report_deps(env))
                .await
                .map_err(|error| error.to_string())?;
            let mut text: Vec<String> = dirs
                .iter()
                .map(|dir| {
                    format!(
                        "Report in {}",
                        dir.join(eval::output::REPORT_MARKDOWN_FILE).display()
                    )
                })
                .collect();
            if reports.len() > 1 {
                text.push(eval::cross_thread_table(&reports));
            }
            Ok(text.join("\n"))
        }
    }
}

fn summary(report: &RunReport) -> String {
    let outcome = &report.outcome;
    let status = serde_json::to_value(outcome.status)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default();
    let verification = outcome.verification.as_ref().map_or("", |verification| {
        if verification.passed {
            ", verification passed"
        } else {
            ", verification failed"
        }
    });
    format!(
        "{}-{}: {status} ({}); {} user turns, {} tokens, {} tool calls, {:.1} s, {} denied, {} pushed refs{verification}",
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

    fn parse(args: &[&str]) -> Result<Command, String> {
        parse_args(args.iter().map(|arg| (*arg).to_owned()))
    }

    #[test]
    fn every_run_flag_is_parsed() {
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
            "--problem",
            "the reviewer missed a bug",
            "--verify",
            "cargo test --offline",
            "--verify-timeout",
            "120",
            "--no-report",
        ];

        // When: it is parsed.
        let command = parse(&argv).unwrap();

        // Then: every value lands in its field and the report step is skipped.
        assert_eq!(
            command,
            Command::Run {
                args: EvalArgs {
                    thread_id: "thread-1".into(),
                    candidate: Some("c.toml".into()),
                    repeat: Some(3),
                    output: Some("out".into()),
                    db: Some("events.db".into()),
                    user_config_dir: Some("config".into()),
                    problem: Some("the reviewer missed a bug".into()),
                    verify: Some("cargo test --offline".into()),
                    verify_timeout: Some(Duration::from_secs(120)),
                },
                report: false,
            }
        );
    }

    #[test]
    fn a_run_needs_only_the_thread_and_reports_by_default() {
        let command = parse(&["eval", "run", "--thread", "t"]).unwrap();
        assert_eq!(
            command,
            Command::Run {
                args: EvalArgs {
                    thread_id: "t".into(),
                    ..EvalArgs::default()
                },
                report: true,
            }
        );
    }

    #[test]
    fn report_takes_several_directories_and_rejudge() {
        // Given / When: a report over two directories that asks the judge again.
        let command = parse(&[
            "eval",
            "report",
            "--output",
            "a",
            "--rejudge",
            "--output",
            "b",
            "--user-config",
            "config",
        ])
        .unwrap();

        // Then: both directories are kept in order.
        assert_eq!(
            command,
            Command::Report(ReportArgs {
                dirs: vec!["a".into(), "b".into()],
                user_config_dir: Some("config".into()),
                rejudge: true,
            })
        );
    }

    #[test]
    fn malformed_command_lines_are_refused_with_usage() {
        for argv in [
            &["eval"][..],
            &["eval", "inspect"],
            &["eval", "report"],
            &["eval", "report", "--rejudge"],
            &["eval", "report", "--output"],
            &["eval", "report", "--output", "a", "--thread", "t"],
            &["eval", "run"],
            &["eval", "run", "--thread"],
            &["eval", "run", "--thread", ""],
            &["eval", "run", "--thread", "t", "--unknown", "x"],
            &["eval", "run", "--thread", "t", "--repeat", "0"],
            &["eval", "run", "--thread", "t", "--repeat", "many"],
            &["eval", "run", "--thread", "t", "--verify-timeout", "0"],
            &["eval", "run", "--thread", "t", "--rejudge"],
        ] {
            let error = parse(argv).unwrap_err();
            assert!(
                error.contains("usage: evorch eval run"),
                "{argv:?}: {error}"
            );
        }
    }
}
