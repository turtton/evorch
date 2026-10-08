//! `evorch inspect`: read-only JSON views of the persisted store for harness diagnosis.
//!
//! The store is opened read-only and never migrated, so inspecting a live GUI's
//! database cannot change it. Output is one pretty-printed JSON document on stdout.

use std::path::PathBuf;

use serde_json::{Value, json};
use storage::Database;
use storage::improvement::{ImprovementCandidate, ImprovementStatus};
use storage::inspect::{EventScope, event_json};

pub const USAGE: &str = "usage: evorch inspect [--db <path>] <command>
  candidates [--project <slug>] [--status new|reviewed|dismissed] [--limit <n>]
  candidate <candidate-id>
  run <run-id> [--full]
  events (--run <run-id> | --around <unix-ns> [--window-ms <ms>]) [--limit <n>]
The default database is <user-config-dir>/evorch-events.db, the GUI's store.";

const DEFAULT_WINDOW_MS: i64 = 5_000;
const DEFAULT_LIMIT: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InspectCommand {
    Candidates {
        project: Option<String>,
        status: Option<ImprovementStatus>,
        limit: usize,
    },
    Candidate(String),
    Run {
        run_id: String,
        full: bool,
    },
    Events {
        scope: Scope,
        limit: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    Run(String),
    Around { at_ns: i64, window_ms: i64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectArgs {
    pub db: Option<PathBuf>,
    pub command: InspectCommand,
}

/// Parses everything after `evorch`, starting with `inspect`.
pub fn parse_args(argv: impl Iterator<Item = String>) -> Result<InspectArgs, String> {
    let mut argv = argv.peekable();
    if argv.next().as_deref() != Some("inspect") {
        return Err(USAGE.into());
    }
    let mut db = None;
    if argv.peek().map(String::as_str) == Some("--db") {
        argv.next();
        db = Some(PathBuf::from(argv.next().ok_or(USAGE)?));
    }
    let command = argv.next().ok_or(USAGE)?;
    let mut positional = Vec::new();
    let mut flags = std::collections::BTreeMap::new();
    let mut full = false;
    while let Some(arg) = argv.next() {
        if arg == "--full" {
            full = true;
        } else if arg.starts_with("--") {
            let value = argv.next().ok_or(USAGE)?;
            if flags.insert(arg, value).is_some() {
                return Err(USAGE.into());
            }
        } else {
            positional.push(arg);
        }
    }
    let mut take = |flag: &str| flags.remove(flag);
    let limit = |value: Option<String>| -> Result<usize, String> {
        value.map_or(Ok(DEFAULT_LIMIT), |value| {
            value
                .parse::<usize>()
                .ok()
                .filter(|limit| *limit > 0)
                .ok_or_else(|| format!("--limit must be a positive integer\n{USAGE}"))
        })
    };
    let command = match (command.as_str(), positional.as_slice(), full) {
        ("candidates", [], false) => InspectCommand::Candidates {
            project: take("--project"),
            status: take("--status")
                .map(|status| {
                    serde_json::from_value(Value::String(status))
                        .map_err(|_| format!("unknown --status\n{USAGE}"))
                })
                .transpose()?,
            limit: limit(take("--limit"))?,
        },
        ("candidate", [id], false) => InspectCommand::Candidate(id.clone()),
        ("run", [run_id], full) => InspectCommand::Run {
            run_id: run_id.clone(),
            full,
        },
        ("events", [], false) => {
            let scope = match (take("--run"), take("--around")) {
                (Some(run_id), None) => Scope::Run(run_id),
                (None, Some(at)) => Scope::Around {
                    at_ns: at
                        .parse()
                        .map_err(|_| format!("--around must be Unix nanoseconds\n{USAGE}"))?,
                    window_ms: take("--window-ms").map_or(Ok(DEFAULT_WINDOW_MS), |ms| {
                        ms.parse::<i64>()
                            .ok()
                            .filter(|ms| *ms >= 0)
                            .ok_or_else(|| format!("--window-ms must be non-negative\n{USAGE}"))
                    })?,
                },
                _ => return Err(USAGE.into()),
            };
            InspectCommand::Events {
                scope,
                limit: limit(take("--limit"))?,
            }
        }
        _ => return Err(USAGE.into()),
    };
    if !flags.is_empty() {
        return Err(USAGE.into());
    }
    Ok(InspectArgs { db, command })
}

/// Runs a parsed command and returns the JSON document to print.
pub fn run(args: InspectArgs) -> Result<Value, String> {
    let path = match args.db {
        Some(path) => path,
        None => config::user_config_dir()
            .ok_or("cannot resolve the user config dir; pass --db")?
            .join("evorch-events.db"),
    };
    if !path.is_file() {
        return Err(format!("database not found: {}", path.display()));
    }
    let db = Database::open_read_only(&path).map_err(|error| error.to_string())?;
    let result = match args.command {
        InspectCommand::Candidates {
            project,
            status,
            limit,
        } => {
            let candidates = db
                .inspect_candidates(project.as_deref(), status, limit)
                .map_err(|error| error.to_string())?;
            json!({ "candidates": candidates.iter().map(candidate_json).collect::<Vec<_>>() })
        }
        InspectCommand::Candidate(id) => {
            let candidate = db
                .improvement_candidate(&id)
                .map_err(|error| error.to_string())?
                .ok_or_else(|| format!("candidate not found: {id}"))?;
            let mut value = candidate_json(&candidate);
            value["next"] = json!(next_steps(&candidate));
            value
        }
        InspectCommand::Run { run_id, full } => db
            .inspect_run(&run_id, full)
            .map_err(|error| error.to_string())?,
        InspectCommand::Events { scope, limit } => {
            let scope_value;
            let (events, truncated) = match &scope {
                Scope::Run(run_id) => {
                    scope_value = json!({ "run_id": run_id });
                    db.inspect_events(EventScope::Run(run_id), limit)
                }
                Scope::Around { at_ns, window_ms } => {
                    let window_ns = window_ms.saturating_mul(1_000_000);
                    let (from_ns, to_ns) = (
                        at_ns.saturating_sub(window_ns),
                        at_ns.saturating_add(window_ns),
                    );
                    scope_value = json!({ "from_ns": from_ns, "to_ns": to_ns });
                    db.inspect_events(EventScope::Window { from_ns, to_ns }, limit)
                }
            }
            .map_err(|error| error.to_string())?;
            json!({
                "scope": scope_value,
                "truncated_older": truncated,
                "events": events.iter().map(event_json).collect::<Vec<_>>(),
            })
        }
    };
    Ok(json!({ "db": path.display().to_string(), "result": result }))
}

fn candidate_json(candidate: &ImprovementCandidate) -> Value {
    let mut value = serde_json::to_value(candidate).unwrap_or(Value::Null);
    // Evidence is stored as text; expand it when it is the collector's JSON object.
    if let Ok(evidence @ Value::Object(_)) = serde_json::from_str::<Value>(&candidate.evidence) {
        value["evidence"] = evidence;
    }
    value
}

/// Follow-up commands that lead from a candidate back to its runs and events.
fn next_steps(candidate: &ImprovementCandidate) -> Vec<String> {
    let mut runs: Vec<&str> = candidate.run_id.iter().map(String::as_str).collect();
    for run in &candidate.recent_run_ids {
        if !runs.contains(&run.as_str()) {
            runs.push(run);
        }
    }
    let mut steps: Vec<String> = runs
        .iter()
        .map(|run| format!("evorch inspect run {run}"))
        .collect();
    if let Some(run) = runs.first() {
        steps.push(format!("evorch inspect events --run {run}"));
    }
    if let Some(at) = serde_json::from_str::<Value>(&candidate.evidence)
        .ok()
        .and_then(|evidence| evidence["observed_at_ns"].as_u64())
    {
        steps.push(format!("evorch inspect events --around {at}"));
    }
    steps
}
