//! `evorch inspect` against a store written by the real storage writer and the
//! self-improvement collector, so a schema or evidence change that breaks the
//! diagnosis path fails here.

use std::path::{Path, PathBuf};
use std::process::Command;

use event_bus::{DiagnosticEvent, DiagnosticSeverity, Event, LifecycleEvent};
use runtime::escalation_review::{
    Decision, Digest, MANIFEST_VERSION, ManifestKind, ModelRequest, Outcome, OutcomeStatus,
    ReviewManifest, VerdictRecord,
};
use runtime::self_improvement::{ImprovementCollector, ImprovementPolicy, ImprovementSettings};
use serde_json::Value;
use storage::usage::{UsageRequestRecord, UsageStatus};
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
        host_inputs_json: "[]".into(),
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

fn review_manifest(run_id: &str) -> ReviewManifest {
    ReviewManifest {
        version: MANIFEST_VERSION,
        kind: ManifestKind::Shell,
        scope: "network_only".into(),
        tool: Some("shell".into()),
        run_id: run_id.into(),
        model_request: Some(ModelRequest {
            category: "tool_execution".into(),
            role: "Reviewer".into(),
            requested_at_ns: 1_000,
            finished_at_ns: Some(2_000),
        }),
        instruction: Some(Digest {
            sha256: "1".repeat(64),
            bytes: 10,
        }),
        schema: None,
        payload: Some(Digest {
            sha256: "2".repeat(64),
            bytes: 20,
        }),
        context: None,
        outcome: Outcome {
            status: OutcomeStatus::Decided,
            verdict: Some(VerdictRecord {
                approve: false,
                risk_level: None,
                authorization_level: None,
                procedure_step: Default::default(),
                decision: Decision::Deny,
            }),
        },
    }
}

fn usage_request(request_id: &str, run_id: &str, at_ns: i64, purpose: &str) -> UsageRequestRecord {
    UsageRequestRecord {
        request_id: request_id.into(),
        at_ns,
        provider: "p".into(),
        profile: None,
        model: "reviewer-model".into(),
        run_id: Some(run_id.into()),
        parent_run_id: None,
        role: None,
        purpose: Some(purpose.into()),
        status: UsageStatus::Ok,
        failure: None,
        finish_reason: None,
        input_tokens: 1,
        output_tokens: 1,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: None,
        ttft_ms: None,
        duration_ms: 1,
        cost_usd: None,
        service_tier: None,
    }
}

#[test]
fn run_shows_each_review_manifest_with_the_requests_that_may_have_served_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("evorch-events.db");
    let storage = Storage::open(StorageConfig {
        db_path: path.clone(),
        ..Default::default()
    })
    .unwrap();
    let handle = storage.handle();
    let manifest = review_manifest("run-7");
    let emit = |run: &str, detail: String, severity| {
        Event::new(DiagnosticEvent {
            source: "sandbox".into(),
            severity,
            code: "escalation_review_manifest".into(),
            detail,
            run_id: Some(run.into()),
            thread_id: Some("thread-1".into()),
            call_id: Some("call-1".into()),
        })
    };
    for event in [
        emit(
            "run-7",
            serde_json::to_string(&manifest).unwrap(),
            DiagnosticSeverity::Info,
        ),
        emit(
            "run-7",
            format!(
                "manifest could not be recorded: boom; payload_sha256={}",
                "3".repeat(64)
            ),
            DiagnosticSeverity::Warning,
        ),
        emit(
            "run-70",
            serde_json::to_string(&review_manifest("run-70")).unwrap(),
            DiagnosticSeverity::Info,
        ),
    ] {
        handle.append_event(Some("evorch-gui"), &event).unwrap();
    }
    handle
        .record_usage_requests(vec![
            usage_request("req-in", "run-7", 1_500, "escalation_review"),
            usage_request("req-before", "run-7", 900, "escalation_review"),
            usage_request("req-agent", "run-7", 1_500, "agent"),
            usage_request("req-other-run", "run-70", 1_500, "escalation_review"),
        ])
        .unwrap();
    storage.close();

    let run = inspect(&path, &["run", "run-7"]).unwrap();
    let manifests = run["review_manifests"].as_array().unwrap();
    assert_eq!(manifests.len(), 2);
    assert_eq!(run["diagnostics"].as_array().unwrap().len(), 0);

    let first = &manifests[0];
    assert_eq!(first["call_id"], "call-1");
    assert_eq!(first["thread_id"], "thread-1");
    assert_eq!(first["manifest"]["payload"]["sha256"], "2".repeat(64));
    assert_eq!(first["manifest"]["outcome"]["verdict"]["decision"], "deny");
    let candidates = first["usage_request_candidates"].as_array().unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0]["request_id"], "req-in");
    assert_eq!(candidates[0]["model"], "reviewer-model");

    let unreadable = &manifests[1];
    assert_eq!(unreadable["severity"], "warning");
    assert!(unreadable["manifest"].is_null());
    assert!(
        unreadable["detail"]
            .as_str()
            .unwrap()
            .contains("could not be recorded")
    );
    assert_eq!(
        unreadable["usage_request_candidates"],
        serde_json::json!([])
    );
}
