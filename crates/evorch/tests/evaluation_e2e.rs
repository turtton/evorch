//! The CLI must report preflight failures without replaying or judging them.

use std::path::Path;
use std::process::Command;

use mock_openai::{StreamingMockOpenAi, WriteMode};
use serde_json::{Value, json};
use storage::thread_start::{ThreadStartCapture, ThreadStartSnapshot, ThreadStartState};
use storage::{ReviewEvidenceRow, RunContextRecord, Storage, StorageConfig};

fn git(project: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args([
            "-c",
            "user.name=evorch-test",
            "-c",
            "user.email=test@evorch.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(project)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap().trim().into()
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn preflight_failure(report_flag: Option<&str>, reports: bool) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let project = root.join("project");
    std::fs::create_dir(&project).unwrap();
    git(&project, &["init", "--quiet", "--initial-branch=main"]);
    std::fs::write(project.join("README"), "replay fixture\n").unwrap();
    git(&project, &["add", "README"]);
    git(&project, &["commit", "--quiet", "-m", "fixture"]);

    let db = root.join("events.db");
    let storage = Storage::open(StorageConfig {
        db_path: db.clone(),
        ..Default::default()
    })
    .unwrap();
    let writer = storage.handle();
    writer
        .save_thread_start_state(ThreadStartState {
            thread_id: "thread-1".into(),
            root_run_id: "run-1".into(),
            project_root: project.to_string_lossy().into_owned(),
            captured_at_ns: 1,
            capture: ThreadStartCapture::Replayable(ThreadStartSnapshot {
                repo_prefix: String::new(),
                head_commit: git(&project, &["rev-parse", "HEAD"]),
                head_ref: Some("refs/heads/main".into()),
                diff: Vec::new(),
                untracked: Vec::new(),
            }),
        })
        .unwrap();
    writer
        .upsert_run_context(&RunContextRecord {
            run_id: "run-1".into(),
            name: format!("chat:{}:thread-1", runtime::Role::Worker.name()),
            role: runtime::Role::Worker.name().into(),
            parent_run_id: None,
            config_json: "{}".into(),
            messages_json: "[]".into(),
            checkpoints_json: "[]".into(),
            terminal_phase: "Done".into(),
            restorable: false,
            updated_at_ns: 1,
            host_inputs_json: "[]".into(),
        })
        .unwrap();
    writer
        .record_review_evidence(&[ReviewEvidenceRow {
            id: "prompt-1".into(),
            root_run_id: "run-1".into(),
            target_run_id: "run-1".into(),
            kind: "initial_prompt".into(),
            text: "fix the bug".into(),
            in_reply_to_question_id: None,
            in_reply_to_title: None,
            reference_kind: None,
            reference_context: None,
            attachments: Vec::new(),
            created_at_ns: 1,
        }])
        .unwrap();
    storage.close();

    let server =
        StreamingMockOpenAi::spawn_with_models(vec![], WriteMode::default(), vec!["gpt-4o".into()]);
    let user = root.join("user");
    std::fs::create_dir(&user).unwrap();
    std::fs::write(
        user.join("config.toml"),
        format!(
            r#"[providers.local]
type = "openai-compatible"
api_protocol = "openai-completions"
base_url = "{}"
api_key_env = "EVAL_TEST_KEY"
models = ["gpt-4o", "missing"]
default_model = "gpt-4o"
[routing.routes]
worker = [{{ profile = "local" }}]
reviewer = [{{ profile = "local" }}]
"#,
            server.base_url()
        ),
    )
    .unwrap();
    let candidate = root.join("candidate.toml");
    std::fs::write(
        &candidate,
        "[routing.routes]\nreviewer = [{ profile = 'local', model = 'missing' }]\n",
    )
    .unwrap();
    // Isolate the real binary's models.dev cache so this test stays offline.
    let cache = root.join("cache/evorch");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::copy(
        test_support::model_catalog_dir().join("models-dev.json"),
        cache.join("models-dev.json"),
    )
    .unwrap();
    let dir = root.join("out");
    let mut command = Command::new(env!("CARGO_BIN_EXE_evorch"));
    command.args(["eval", "run", "--thread", "thread-1", "--repeat", "1"]);
    if let Some(flag) = report_flag {
        command.arg(flag);
    }
    let output = command
        .arg("--db")
        .arg(&db)
        .arg("--user-config")
        .arg(&user)
        .arg("--candidate")
        .arg(candidate)
        .arg("--output")
        .arg(&dir)
        .env("HOME", &root)
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("EVAL_TEST_KEY", "eval-fixture-key")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{output:?}");
    assert!(stderr.contains("evaluation preflight failed"), "{stderr}");
    assert!(stderr.contains("missing"), "{stderr}");
    assert_eq!(read_json(&dir.join("manifest.json"))["runs"], json!([]));
    assert!(!dir.join("runs").exists());
    assert_eq!(dir.join("report.json").exists(), reports, "{stderr}");
    assert_eq!(dir.join("report.md").exists(), reports, "{stderr}");
    if reports {
        let report = read_json(&dir.join("report.json"));
        assert_eq!(report["pairs"], json!([]));
        assert!(
            report["warnings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|warning| {
                    warning["kind"] == "preflight_failed"
                        && warning["message"].as_str().unwrap().contains("missing")
                }),
            "{report}"
        );
        let markdown = std::fs::read_to_string(dir.join("report.md")).unwrap();
        assert!(markdown.contains("no samples compared"), "{markdown}");
        assert!(markdown.contains("missing"), "{markdown}");
        assert!(stderr.contains("Report in"), "{stderr}");
    }
    let requests = server.recorded_requests();
    assert_eq!(requests.len(), 3, "{requests:?}");
    assert!(
        requests.iter().all(|request| request.method == "GET"),
        "no replay or judge inference: {requests:?}"
    );
}

#[test]
fn preflight_failure_writes_report_and_exits_nonzero() {
    preflight_failure(None, true);
}

#[test]
fn explicit_report_preserves_preflight_failure_exit() {
    preflight_failure(Some("--report"), true);
}

#[test]
fn no_report_preserves_preflight_failure_without_reports() {
    preflight_failure(Some("--no-report"), false);
}
