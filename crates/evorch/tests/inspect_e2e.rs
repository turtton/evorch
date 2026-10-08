//! `evorch inspect` against a store written by the real storage writer and the
//! self-improvement collector, so a schema or evidence change that breaks the
//! diagnosis path fails here.

use std::path::{Path, PathBuf};
use std::process::Command;

use event_bus::{DiagnosticEvent, DiagnosticSeverity, Event, LifecycleEvent};
use runtime::self_improvement::{ImprovementCollector, ImprovementPolicy, ImprovementSettings};
use serde_json::Value;
use storage::{RunContextRecord, Storage, StorageConfig};

fn inspect(db: &Path, args: &[&str]) -> Result<Value, String> {
    let output = Command::new(env!("CARGO_BIN_EXE_evorch"))
        .arg("inspect")
        .arg("--db")
        .arg(db)
        .args(args)
        .output()
        .unwrap();
    if output.status.success() {
        Ok(serde_json::from_slice::<Value>(&output.stdout).unwrap()["result"].clone())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).into_owned())
    }
}

fn context(run_id: &str, parent: Option<&str>, messages: &str) -> RunContextRecord {
    RunContextRecord {
        run_id: run_id.into(),
        role: "worker".into(),
        name: format!("{run_id}-name"),
        parent_run_id: parent.map(Into::into),
        config_json: "{}".into(),
        messages_json: messages.into(),
        checkpoints_json: "[]".into(),
        terminal_phase: "Error".into(),
        restorable: false,
        updated_at_ns: 1,
    }
}

/// A store holding one harness diagnostic for run-7, a child run-8 and an unrelated run-70.
fn fixture(dir: &Path) -> (PathBuf, Event) {
    let path = dir.join("evorch-events.db");
    let storage = Storage::open(StorageConfig {
        db_path: path.clone(),
        ..Default::default()
    })
    .unwrap();
    let handle = storage.handle();
    let diagnostic = DiagnosticEvent {
        source: "budget_tracker".into(),
        severity: DiagnosticSeverity::Warning,
        code: "NoProgress".into(),
        detail: "no progress across 5 turns".into(),
        run_id: Some("run-7".into()),
        thread_id: None,
        call_id: None,
    };
    let event = Event::new(diagnostic.clone());
    for event in [
        event.clone(),
        Event::new(LifecycleEvent::AgentRunStarted {
            run_id: "run-8".into(),
            parent_run_id: Some("run-7".into()),
            agent_name: "child".into(),
            role: "worker".into(),
        }),
        Event::new(DiagnosticEvent {
            run_id: Some("run-70".into()),
            ..diagnostic.clone()
        }),
    ] {
        handle.append_event(Some("evorch-gui"), &event).unwrap();
    }
    handle
        .upsert_run_context(&context(
            "run-7",
            None,
            r#"[{"role":"user","content":"go"},{"role":"assistant","content":"stuck"}]"#,
        ))
        .unwrap();
    handle
        .upsert_run_context(&context("run-8", Some("run-7"), "[]"))
        .unwrap();
    handle
        .append_run_ledger("run-7", r#"{"step":"tried"}"#)
        .unwrap();
    ImprovementCollector::new(ImprovementSettings {
        writer: handle,
        project: "p".into(),
        policy: ImprovementPolicy {
            draft_dir: Some(dir.join("drafts")),
            ..Default::default()
        },
    })
    .handle_diagnostic(&diagnostic);
    storage.close();
    (path, event)
}

#[test]
fn candidate_leads_back_to_its_run_and_events() {
    let dir = tempfile::tempdir().unwrap();
    let (db, _) = fixture(dir.path());

    let listed = inspect(&db, &["candidates", "--status", "new"]).unwrap();
    let candidates = listed["candidates"].as_array().unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0]["project"], "p");
    // Collector evidence is expanded from its stored JSON text.
    assert_eq!(candidates[0]["evidence"]["code"], "NoProgress");
    let id = candidates[0]["id"].as_str().unwrap();

    let candidate = inspect(&db, &["candidate", id]).unwrap();
    let observed = candidate["evidence"]["observed_at_ns"].as_u64().unwrap();
    let next: Vec<_> = candidate["next"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| step.as_str().unwrap())
        .collect();
    assert_eq!(
        next,
        [
            "evorch inspect run run-7".to_owned(),
            "evorch inspect events --run run-7".to_owned(),
            format!("evorch inspect events --around {observed}"),
        ]
    );

    let run = inspect(&db, &["run", "run-7"]).unwrap();
    assert_eq!(run["context"]["messages_count"], 2);
    assert!(run["context"].get("messages_json").is_none());
    assert_eq!(run["children"][0]["run_id"], "run-8");
    assert_eq!(run["ledger"][0]["body"]["step"], "tried");
    // Only run-7's own events count: not the child's start event, not run-70.
    assert_eq!(run["event_kinds"], serde_json::json!({ "Diagnostic": 1 }));
    assert_eq!(
        run["diagnostics"][0]["payload"]["payload"]["code"],
        "NoProgress"
    );
    assert_eq!(run["improvement_candidates"][0]["candidate_id"], id);
    let full = inspect(&db, &["run", "run-7", "--full"]).unwrap();
    assert_eq!(full["context"]["messages_json"][1]["content"], "stuck");

    let events = inspect(&db, &["events", "--run", "run-7"]).unwrap();
    assert_eq!(events["events"].as_array().unwrap().len(), 1);
    assert_eq!(events["truncated_older"], false);
    // Direct intake stamps intake time; bus intake stamps the event's own wall clock.
    let around = inspect(&db, &["events", "--around", &observed.to_string()]).unwrap();
    assert!(
        around["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["payload"]["payload"]["run_id"] == "run-7")
    );
    let latest = inspect(
        &db,
        &["events", "--around", &observed.to_string(), "--limit", "1"],
    )
    .unwrap();
    assert_eq!(latest["events"].as_array().unwrap().len(), 1);
    assert_eq!(latest["truncated_older"], true);
}

#[test]
fn inspection_never_creates_or_migrates_a_store() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.db");
    assert!(
        inspect(&missing, &["candidates"])
            .unwrap_err()
            .contains("database not found")
    );
    assert!(!missing.exists());

    let (db, _) = fixture(dir.path());
    let connection = rusqlite::Connection::open(&db).unwrap();
    let current: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    connection
        .pragma_update(None, "user_version", current - 1)
        .unwrap();
    drop(connection);
    let error = inspect(&db, &["candidates"]).unwrap_err();
    assert!(
        error.contains("open it once with the current evorch"),
        "{error}"
    );
    let version: u32 = rusqlite::Connection::open(&db)
        .unwrap()
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, current - 1);
}

#[test]
fn malformed_arguments_print_usage() {
    let dir = tempfile::tempdir().unwrap();
    let (db, _) = fixture(dir.path());
    for args in [
        &["events"][..],
        &["events", "--run", "run-7", "--around", "1"],
        &["candidate"],
        &["candidates", "--status", "open"],
        &["run", "run-7", "--bogus", "x"],
    ] {
        assert!(
            inspect(&db, args)
                .unwrap_err()
                .contains("usage: evorch inspect"),
            "{args:?}"
        );
    }
}
