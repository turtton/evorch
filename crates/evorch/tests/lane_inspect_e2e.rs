//! `evorch inspect run` with the lane section, against a store written by the real
//! storage writer. The marker of the run snapshot is serialized by the runtime's own type.

use std::path::Path;
use std::process::Command;

use event_bus::lane::{LaneBrief, LanePolicy};
use runtime::budget_tracker::SpawnCredits;
use serde_json::{Value, json};
use storage::lanes::NewLane;
use storage::threads::ThreadUpsert;
use storage::{RunContextRecord, Storage, StorageConfig};

fn inspect(db: &Path, args: &[&str]) -> Result<Value, String> {
    let output = Command::new(std::env::var("CARGO_BIN_EXE_evorch").expect("CARGO_BIN_EXE_evorch"))
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

fn lead_context(config: Value) -> RunContextRecord {
    RunContextRecord {
        run_id: "lead-root".into(),
        role: "Orchestrator".into(),
        name: "chat:Orchestrator:lead".into(),
        parent_run_id: None,
        config_json: config.to_string(),
        messages_json: r#"[{"role":"user","content":"go"}]"#.into(),
        checkpoints_json: "[]".into(),
        terminal_phase: "Checkpoint".into(),
        restorable: true,
        updated_at_ns: 1,
        host_inputs_json: "[]".into(),
    }
}

fn store(dir: &Path, config: Value) -> std::path::PathBuf {
    let path = dir.join("evorch-events.db");
    let storage = Storage::open(StorageConfig {
        db_path: path.clone(),
        ..Default::default()
    })
    .unwrap();
    let handle = storage.handle();
    handle
        .upsert_threads_host(&[ThreadUpsert {
            thread_id: "lead".into(),
            root_run_id: Some("lead-root".into()),
            chat_role: Some("Orchestrator".into()),
            ..Default::default()
        }])
        .unwrap();
    handle.upsert_run_context(&lead_context(config)).unwrap();
    handle
        .create_lane(NewLane {
            lane_id: "lane-1".into(),
            thread_id: "thread-1".into(),
            origin_thread_id: "lead".into(),
            origin_run_id: "lead-root".into(),
            creation_key: "open-1".into(),
            brief: LaneBrief {
                verification_requirements: Default::default(),
                objective: "private objective".into(),
                inputs: vec![],
                deliverables: vec![],
                acceptance_criteria: vec!["verified".into()],
                constraints: vec![],
                workspace: Default::default(),
                report_policy: LanePolicy::Attached,
            },
            start_seed: "seed".into(),
            project_id: None,
            project_root: None,
            title: None,
            at_ns: 1,
        })
        .unwrap();
    storage.close();
    path
}

#[test]
fn run_adds_the_lane_section_with_the_marker_the_runtime_serialized() {
    let dir = tempfile::tempdir().unwrap();
    let config = json!({"spawn_credits": serde_json::to_value(SpawnCredits::default()).unwrap()});
    let db = store(dir.path(), config);

    let run = inspect(&db, &["run", "lead-root"]).unwrap();

    let lanes = &run["lane_diagnostics"];
    assert_eq!(lanes["scope"]["managed_lanes_attributed"], true);
    assert_eq!(lanes["lanes"]["items"][0]["lane_id"], "lane-1");
    let stored = &lanes["uncollected"]["stored"];
    assert_eq!(stored["available"], true);
    assert_eq!(stored["uncollected_lane_open_count"], 0);
    assert_eq!(stored["lane_ids_total"], 0);
    assert_eq!(stored["suppression_reported"], false);
    assert_eq!(
        lanes["uncollected"]["stored_marker_suppresses_creation_now"],
        false
    );
    assert_eq!(lanes["related_diagnostics"]["events"], json!([]));
    assert!(!run.to_string().contains("private objective"));
}

#[test]
fn a_snapshot_without_the_marker_is_unavailable_not_zero() {
    let dir = tempfile::tempdir().unwrap();
    let db = store(dir.path(), json!({}));

    let run = inspect(&db, &["run", "lead-root"]).unwrap();

    let stored = &run["lane_diagnostics"]["uncollected"]["stored"];
    assert_eq!(stored["available"], false);
    assert!(stored["reason"].as_str().unwrap().contains("spawn_credits"));
    assert_eq!(stored.get("uncollected_lane_open_count"), None);
    assert_eq!(
        run["lane_diagnostics"]["uncollected"]["stored_marker_suppresses_creation_now"],
        Value::Null
    );
    assert_eq!(
        run["lane_diagnostics"]["lanes"]["items"][0]["lane_id"],
        "lane-1"
    );
}

#[test]
fn an_unknown_run_has_an_empty_lane_section_and_no_error() {
    let dir = tempfile::tempdir().unwrap();
    let db = store(dir.path(), json!({}));

    let run = inspect(&db, &["run", "unknown-run"]).unwrap();

    assert_eq!(run["context"], Value::Null);
    assert_eq!(run["lane_diagnostics"]["lanes"]["total"], 0);
    assert_eq!(
        run["lane_diagnostics"]["uncollected"]["stored"]["available"],
        false
    );
}
