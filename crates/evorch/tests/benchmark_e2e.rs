//! Frozen local comparisons through configured providers and the real runtime.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use event_bus::AgentRunPhase;
use evorch::benchmark::{parse_args, read_report, record, replay};
use evorch::headless::SandboxChoice;
use mock_openai::{ScriptedResponse, StreamingMockOpenAi};
use routing::MapEnv;
use runtime::ModelPreference;
use serde_json::json;

const MODEL: &str = "gpt-4o";
const CANDIDATE: &str = "gpt-4o-mini";
const KEY_ENV: &str = "EVORCH_TEST_BENCHMARK_KEY";

fn call(id: &str, model: &str, tool: &str, input: serde_json::Value) -> ScriptedResponse {
    ScriptedResponse::tool_call(id, model, 0, id, tool, [input.to_string()]).with_usage(2, 1)
}

fn text(id: &str, model: &str, text: &str) -> ScriptedResponse {
    ScriptedResponse::text_stream(id, model, [text]).with_usage(2, 1)
}

fn fixture(root: &Path, base_url: &str) -> PathBuf {
    let fixture = root.join("fixture");
    std::fs::create_dir_all(fixture.join(".evorch")).unwrap();
    std::fs::create_dir(fixture.join(".cargo")).unwrap();
    std::fs::write(fixture.join("answer.txt"), "initial checkpoint contents").unwrap();
    std::fs::write(
        fixture.join("verify.sh"),
        "test ! -e target/poison && grep -qx good answer.txt\n",
    )
    .unwrap();
    std::fs::write(fixture.join(".gitignore"), "ignored.txt\n").unwrap();
    std::fs::write(fixture.join("ignored.txt"), "restore ignored input too").unwrap();
    std::fs::write(
        config::project_main_config_path(&fixture),
        format!(
            r#"
[providers.local]
type = "openai-compatible"
base_url = "{base_url}"
api_key_env = "{KEY_ENV}"
models = ["{MODEL}", "{CANDIDATE}"]
default_model = "{MODEL}"
[[routing.routes.worker]]
profile = "local"
[[routing.routes.orchestrator]]
profile = "local"
[agents.worker.categories.quick.generation]
temperature = 0.25
max_tokens = 321
"#
        ),
    )
    .unwrap();
    let path = root.join("task.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&json!({
        "fixture_dir": "fixture", "root_prompt": "Complete the fixed task with sequential worker delegation",
        "target": {"role":"worker", "category":"quick", "occurrence":1},
        "verifier": {"program":"sh", "args":["verify.sh"], "protected_paths":["verify.sh",".gitignore"], "allowed_changed_paths":["answer.txt"]},
        "budget": {"max_tokens":1000, "max_tool_calls":20, "timeout_seconds":300}
    })).unwrap()).unwrap();
    path
}

fn env() -> Arc<MapEnv> {
    Arc::new(MapEnv::from_iter([(KEY_ENV, "local-mock-key")]))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn baseline_then_frozen_replays_keep_inputs_and_evaluate_trusted_tests() {
    let root = tempfile::tempdir().unwrap();
    let mock = StreamingMockOpenAi::spawn_with_models(
        vec![
            call(
                "delegate-first",
                MODEL,
                "delegate",
                json!({"target":{"role":"worker","category":"quick"},"prompt":"Implement the answer"}),
            ),
            call(
                "baseline-write",
                MODEL,
                "write",
                json!({"path":"answer.txt","content":"bad"}),
            ),
            text("baseline-local", MODEL, "first implementation complete"),
            call(
                "delegate-fix",
                MODEL,
                "delegate",
                json!({"target":{"role":"worker","category":"quick"},"prompt":"FUTURE_REVIEW_EVIDENCE: fix bad to good"}),
            ),
            call(
                "baseline-fix",
                MODEL,
                "write",
                json!({"path":"answer.txt","content":"good"}),
            ),
            text("baseline-fix-result", MODEL, "repair complete"),
            text(
                "baseline-final",
                MODEL,
                "BASELINE_FINAL_EVIDENCE after review and repair",
            ),
            call(
                "same-model-write",
                MODEL,
                "write",
                json!({"path":"answer.txt","content":"bad"}),
            ),
            text("same-model-final", MODEL, "first implementation complete"),
            call(
                "candidate-future-read",
                CANDIDATE,
                "read",
                json!({"path":"../baseline.json"}),
            ),
            call(
                "candidate-read",
                CANDIDATE,
                "read",
                json!({"path":"answer.txt"}),
            ),
            call(
                "candidate-write",
                CANDIDATE,
                "write",
                json!({"path":"answer.txt","content":"good"}),
            ),
            call(
                "candidate-build-cache",
                CANDIDATE,
                "shell",
                json!({"command":"mkdir -p target && printf poison > target/poison"}),
            ),
            text("candidate-final", CANDIDATE, "candidate complete"),
            call(
                "tamper",
                CANDIDATE,
                "write",
                json!({"path":"verify.sh","content":"exit 0\n"}),
            ),
            text("tamper-final", CANDIDATE, "candidate claims success"),
            call(
                "config-spoof",
                CANDIDATE,
                "write",
                json!({"path":".cargo/config.toml", "content":"[target.x86_64-unknown-linux-gnu]\nrunner = '/bin/true'\n"}),
            ),
            text(
                "config-spoof-final",
                CANDIDATE,
                "candidate claims success again",
            ),
        ],
        mock_openai::WriteMode::default(),
        vec![MODEL.into(), CANDIDATE.into()],
    );
    let spec = fixture(root.path(), &mock.base_url());
    let output = root.path().join("result");
    let user = Some(root.path().join("user"));
    let baseline = record(
        &spec,
        &output,
        user.clone(),
        env(),
        SandboxChoice::DirectUnchecked,
    )
    .await
    .unwrap();
    assert_eq!(baseline.full.phase, AgentRunPhase::Done);
    assert!(
        baseline.full.evaluation.passed,
        "{}; final={:?}; local={:?}; events={}",
        baseline.full.evaluation.output,
        baseline.full.final_text,
        baseline.local,
        serde_json::from_slice::<Vec<event_bus::Event>>(
            &std::fs::read(output.join("baseline-events.json")).unwrap()
        )
        .unwrap()
        .iter()
        .filter_map(|e| {
            if let event_bus::EventKind::Lifecycle(
                event_bus::LifecycleEvent::AgentRunStateChanged {
                    run_id, to, reason, ..
                },
            ) = &e.kind
            {
                Some(format!("{run_id}:{to:?}:{reason:?}"))
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
    );
    assert!(!baseline.local.as_ref().unwrap().evaluation.passed);
    assert_eq!(
        baseline.checkpoint.as_ref().unwrap().category.as_deref(),
        Some("quick")
    );
    assert_eq!(baseline.local.as_ref().unwrap().metrics.input_tokens, 4);
    assert_eq!(
        std::fs::read_to_string(root.path().join("fixture/answer.txt")).unwrap(),
        "initial checkpoint contents"
    );
    let candidate = ModelPreference {
        profile: "local".into(),
        model: Some(CANDIDATE.into()),
        reasoning_effort: None,
    };
    let same = replay(
        &output,
        "same-model",
        ModelPreference {
            profile: "local".into(),
            model: Some(MODEL.into()),
            reasoning_effort: None,
        },
        user.clone(),
        env(),
        SandboxChoice::DirectUnchecked,
    )
    .await
    .unwrap();
    assert_eq!(same.phase, AgentRunPhase::Done);
    assert_eq!(same.diff, baseline.local.as_ref().unwrap().diff);
    assert_eq!(same.final_text, baseline.local.as_ref().unwrap().final_text);
    assert!(!same.evaluation.passed);
    let invalid = replay(
        &output,
        "unconfigured",
        ModelPreference {
            profile: "missing".into(),
            model: Some(CANDIDATE.into()),
            reasoning_effort: None,
        },
        user.clone(),
        env(),
        SandboxChoice::DirectUnchecked,
    )
    .await
    .unwrap();
    assert_eq!(invalid.phase, AgentRunPhase::Error);
    assert!(invalid.error.is_some());
    assert!(output.join("trials/unconfigured/trial.json").is_file());
    let good = replay(
        &output,
        "candidate",
        candidate.clone(),
        user.clone(),
        env(),
        SandboxChoice::DirectUnchecked,
    )
    .await
    .unwrap();
    assert_eq!(good.phase, AgentRunPhase::Done, "{:?}", good.error);
    assert!(good.evaluation.passed, "{}", good.evaluation.output);
    assert!(
        output.join("workspace/target/poison").is_file(),
        "verification used a fresh fixture despite poisoned candidate build output"
    );
    assert!(good.diff.contains("+good"));
    assert_eq!(
        std::fs::read_to_string(output.join("workspace/ignored.txt")).unwrap(),
        "restore ignored input too"
    );
    let tampered = replay(
        &output,
        "tampered",
        candidate.clone(),
        user,
        env(),
        SandboxChoice::DirectUnchecked,
    )
    .await
    .unwrap();
    assert!(!tampered.evaluation.passed);
    assert_eq!(
        tampered.evaluation.verifier_tampered,
        vec![PathBuf::from("verify.sh")]
    );
    let spoofed = replay(
        &output,
        "config-spoof",
        candidate.clone(),
        None,
        env(),
        SandboxChoice::DirectUnchecked,
    )
    .await
    .unwrap();
    assert!(!spoofed.evaluation.passed);
    assert_eq!(
        spoofed.evaluation.unexpected_changes,
        vec![PathBuf::from(".cargo/config.toml")]
    );
    assert!(
        replay(
            &output,
            "candidate",
            candidate,
            None,
            env(),
            SandboxChoice::DirectUnchecked
        )
        .await
        .is_err(),
        "existing trial is never overwritten"
    );
    let report = read_report(&output).unwrap();
    assert_eq!(report.trials.len(), 5);
    assert!(report.markdown().contains("BASELINE_FINAL_EVIDENCE"));
    assert!(report.markdown().contains("first implementation complete"));
    std::fs::create_dir_all(output.join("workspace/nested/.git")).unwrap();
    assert!(
        replay(
            &output,
            "git-metadata",
            ModelPreference {
                profile: "local".into(),
                model: Some(CANDIDATE.into()),
                reasoning_effort: None,
            },
            None,
            env(),
            SandboxChoice::DirectUnchecked
        )
        .await
        .unwrap_err()
        .to_string()
        .contains(".git-free")
    );
    let requests = mock.recorded_requests();
    let completions: Vec<_> = requests
        .iter()
        .filter(|request| request.path == "/v1/chat/completions")
        .collect();
    assert_eq!(completions.len(), 18);
    let baseline_input = &completions[1].body;
    for key in ["messages", "tools", "temperature", "max_tokens", "model"] {
        assert_eq!(
            baseline_input[key], completions[7].body[key],
            "A/A fixed {key}"
        );
    }
    let replay_input = &completions[9].body;
    assert_eq!(replay_input["model"], CANDIDATE);
    for key in ["messages", "tools", "temperature", "max_tokens"] {
        assert_eq!(baseline_input[key], replay_input[key], "fixed {key}");
    }
    assert_eq!(replay_input["temperature"], 0.25);
    assert_eq!(replay_input["max_tokens"], 321);
    let replay_input_text = replay_input.to_string();
    assert!(!replay_input_text.contains("BASELINE_FINAL_EVIDENCE"));
    assert!(!replay_input_text.contains("FUTURE_REVIEW_EVIDENCE"));
    assert!(
        completions[11].body["messages"]
            .to_string()
            .contains("initial checkpoint contents")
    );
    assert!(
        !completions[10].body["messages"]
            .to_string()
            .contains("BASELINE_FINAL_EVIDENCE"),
        "filesystem future evidence is also blocked"
    );
}

#[test]
fn command_and_spec_errors_fail_before_execution() {
    let args = [
        "benchmark",
        "replay",
        "--record",
        "r",
        "--trial",
        "../escape",
        "--profile",
        "p",
        "--model",
        "m",
    ];
    assert!(parse_args(args.into_iter().map(str::to_owned)).is_err());
    assert!(
        parse_args(
            [
                "benchmark",
                "record",
                "--spec",
                "s",
                "--output",
                "o",
                "--unknown",
                "x"
            ]
            .into_iter()
            .map(str::to_owned)
        )
        .is_err()
    );
    let root = tempfile::tempdir().unwrap();
    let spec = fixture(root.path(), "http://127.0.0.1:1/v1");
    let mut json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&spec).unwrap()).unwrap();
    for (role, category, accepted) in [
        ("reviewer", Some("plan-review"), true),
        ("explorer", None, true),
        ("worker", Some("plan-review"), false),
        ("reviewer", Some("quick"), false),
        ("orchestrator", None, false),
    ] {
        json["target"] = json!({"role":role,"category":category,"occurrence":1});
        std::fs::write(&spec, serde_json::to_vec(&json).unwrap()).unwrap();
        assert_eq!(
            evorch::benchmark::TaskSpec::load(&spec).is_ok(),
            accepted,
            "target role/category {role}/{category:?}"
        );
    }
    json["target"] = json!({"role":"worker","category":"quick","occurrence":1});
    json["verifier"]["protected_paths"] = json!(["../verify.sh"]);
    std::fs::write(&spec, serde_json::to_vec(&json).unwrap()).unwrap();
    assert!(evorch::benchmark::TaskSpec::load(&spec).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn final_usage_over_budget_poison_is_durable_and_skips_verification() {
    let root = tempfile::tempdir().unwrap();
    let mock = StreamingMockOpenAi::spawn_with_models(
        vec![
            call(
                "delegate",
                MODEL,
                "delegate",
                json!({"target":{"role":"worker","category":"quick"},"prompt":"Inspect the answer"}),
            ),
            text("worker", MODEL, "local complete"),
            text("root", MODEL, "full complete"),
        ],
        mock_openai::WriteMode::default(),
        vec![MODEL.into(), CANDIDATE.into()],
    );
    let spec = fixture(root.path(), &mock.base_url());
    let mut task: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&spec).unwrap()).unwrap();
    task["budget"]["max_tokens"] = json!(8);
    std::fs::write(&spec, serde_json::to_vec(&task).unwrap()).unwrap();
    let output = root.path().join("record");
    let baseline = record(
        &spec,
        &output,
        Some(root.path().join("user")),
        env(),
        SandboxChoice::DirectUnchecked,
    )
    .await
    .unwrap();
    assert_eq!(baseline.full.phase, AgentRunPhase::Stopped);
    assert_eq!(
        baseline.full.metrics.input_tokens + baseline.full.metrics.output_tokens,
        9
    );
    assert!(
        baseline
            .poisoned
            .as_deref()
            .unwrap()
            .contains("token budget")
    );
    assert!(baseline.full.diff.is_empty());
    assert!(
        baseline
            .full
            .evaluation
            .output
            .contains("verification skipped")
    );
    std::fs::remove_file(output.join("poisoned.json")).unwrap();
    let error = replay(
        &output,
        "unsafe",
        ModelPreference {
            profile: "local".into(),
            model: Some(CANDIDATE.into()),
            reasoning_effort: None,
        },
        None,
        env(),
        SandboxChoice::DirectUnchecked,
    )
    .await
    .unwrap_err();
    assert!(
        error.to_string().contains("poisoned"),
        "marker deletion cannot restore an interrupted record"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incomplete_candidate_poison_blocks_later_trials_without_snapshot_or_verifier() {
    let root = tempfile::tempdir().unwrap();
    let mock = StreamingMockOpenAi::spawn_with_models(
        vec![
            call(
                "delegate",
                MODEL,
                "delegate",
                json!({"target":{"role":"worker","category":"quick"},"prompt":"Inspect the answer"}),
            ),
            text("worker", MODEL, "local complete"),
            text("root", MODEL, "full complete"),
            call(
                "side-channel",
                CANDIDATE,
                "send",
                json!({"target":"run-1","message":"Need parent state"}),
            ),
        ],
        mock_openai::WriteMode::default(),
        vec![MODEL.into(), CANDIDATE.into()],
    );
    let spec = fixture(root.path(), &mock.base_url());
    let output = root.path().join("record");
    record(
        &spec,
        &output,
        Some(root.path().join("user")),
        env(),
        SandboxChoice::DirectUnchecked,
    )
    .await
    .unwrap();
    let candidate = ModelPreference {
        profile: "local".into(),
        model: Some(CANDIDATE.into()),
        reasoning_effort: None,
    };
    let trial = replay(
        &output,
        "unsupported",
        candidate.clone(),
        None,
        env(),
        SandboxChoice::DirectUnchecked,
    )
    .await
    .unwrap();
    assert_eq!(trial.phase, AgentRunPhase::Error);
    assert!(trial.error.as_deref().unwrap().contains("side-channel"));
    assert!(trial.diff.is_empty());
    assert!(trial.evaluation.output.contains("verification skipped"));
    assert!(output.join("trials/unsupported/events.json").is_file());
    assert!(read_report(&output).unwrap().baseline.poisoned.is_some());
    std::fs::remove_file(output.join("poisoned.json")).unwrap();
    assert!(
        replay(
            &output,
            "next",
            candidate,
            None,
            env(),
            SandboxChoice::DirectUnchecked
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("poisoned")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn executable_trusted_verifier_keeps_large_output_artifacts_after_temp_workspace_cleanup() {
    use std::os::unix::fs::PermissionsExt;
    const CHILD_FLAG: &str = "EVORCH_BENCHMARK_VERIFIER_ARTIFACT_FIXTURE";
    if std::env::var_os(CHILD_FLAG).is_none() {
        // Isolate the bounded output store without mutating process-global
        // environment in the other concurrent integration tests.
        let output_store = tempfile::tempdir().unwrap();
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "executable_trusted_verifier_keeps_large_output_artifacts_after_temp_workspace_cleanup",
                "--nocapture",
            ])
            .env(CHILD_FLAG, "1")
            .env("EVORCH_OUTPUT_DIR", output_store.path())
            .output()
            .unwrap();
        assert!(
            child.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );
        assert!(String::from_utf8_lossy(&child.stdout).contains("1 passed"));
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let mock = StreamingMockOpenAi::spawn_with_models(
        vec![
            call(
                "delegate",
                MODEL,
                "delegate",
                json!({"target":{"role":"worker","category":"quick"},"prompt":"Inspect the answer"}),
            ),
            text("worker", MODEL, "local complete"),
            text("root", MODEL, "full complete"),
        ],
        mock_openai::WriteMode::default(),
        vec![MODEL.into(), CANDIDATE.into()],
    );
    let spec = fixture(root.path(), &mock.base_url());
    let script = root.path().join("fixture/verify.sh");
    std::fs::write(&script, "#!/bin/sh\ni=0\nwhile [ \"$i\" -lt 400 ]; do printf 'large-verifier-line-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\\n'; i=$((i + 1)); done\ngrep -qx 'initial checkpoint contents' answer.txt\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut task: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&spec).unwrap()).unwrap();
    task["verifier"]["program"] = json!("./verify.sh");
    task["verifier"]["args"] = json!([]);
    std::fs::write(&spec, serde_json::to_vec(&task).unwrap()).unwrap();
    let output = root.path().join("record");
    let baseline = record(
        &spec,
        &output,
        Some(root.path().join("user")),
        env(),
        SandboxChoice::DirectUnchecked,
    )
    .await
    .unwrap();
    for trial in [&baseline.full, baseline.local.as_ref().unwrap()] {
        assert!(trial.evaluation.passed, "{}", trial.evaluation.output);
        let reference = trial
            .evaluation
            .output
            .split("[Output artifact: ")
            .nth(1)
            .unwrap_or_else(|| {
                panic!(
                    "expected durable verifier artifact: {}",
                    trial.evaluation.output
                )
            })
            .split(';')
            .next()
            .unwrap();
        let artifact = Path::new(reference);
        assert!(artifact.starts_with(output.join("verifier-artifacts")));
        assert!(
            artifact.is_file(),
            "published verifier reference must survive TempDir cleanup"
        );
        assert!(
            std::fs::read_to_string(artifact)
                .unwrap()
                .contains("large-verifier-line")
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_nonselected_delegate_poison_survives_parent_recovery_and_selected_success() {
    use std::io::Write;
    let root = tempfile::tempdir().unwrap();
    let failed = StreamingMockOpenAi::spawn_with_models(
        vec![],
        mock_openai::WriteMode::default(),
        vec![MODEL.into()],
    );
    let mock = StreamingMockOpenAi::spawn_with_models(
        vec![
            call(
                "delegate-explorer",
                MODEL,
                "delegate",
                json!({"target":{"role":"explorer"},"prompt":"Explore first"}),
            ),
            call(
                "delegate-worker",
                MODEL,
                "delegate",
                json!({"target":{"role":"worker","category":"quick"},"prompt":"Continue after failed exploration"}),
            ),
            text("worker", MODEL, "selected leaf complete"),
            text("root", MODEL, "parent recovered and completed"),
        ],
        mock_openai::WriteMode::default(),
        vec![MODEL.into(), CANDIDATE.into()],
    );
    let spec = fixture(root.path(), &mock.base_url());
    let mut config = std::fs::OpenOptions::new()
        .append(true)
        .open(config::project_main_config_path(
            &root.path().join("fixture"),
        ))
        .unwrap();
    writeln!(config, "\n[providers.failed]\ntype = \"openai-compatible\"\nbase_url = \"{}\"\napi_key_env = \"{KEY_ENV}\"\nmodels = [\"{MODEL}\"]\ndefault_model = \"{MODEL}\"\n[[routing.routes.explorer]]\nprofile = \"failed\"", failed.base_url()).unwrap();
    let output = root.path().join("record");
    let baseline = record(
        &spec,
        &output,
        Some(root.path().join("user")),
        env(),
        SandboxChoice::DirectUnchecked,
    )
    .await
    .unwrap();
    assert_eq!(baseline.full.phase, AgentRunPhase::Done);
    assert_eq!(baseline.local.as_ref().unwrap().phase, AgentRunPhase::Done);
    assert!(
        baseline
            .poisoned
            .as_deref()
            .unwrap()
            .contains("run-2 Error")
    );
    assert!(baseline.full.diff.is_empty());
    assert!(
        baseline
            .full
            .evaluation
            .output
            .contains("verification skipped")
    );
    assert!(
        replay(
            &output,
            "unsafe",
            ModelPreference {
                profile: "local".into(),
                model: Some(CANDIDATE.into()),
                reasoning_effort: None,
            },
            None,
            env(),
            SandboxChoice::DirectUnchecked
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("poisoned")
    );
}
