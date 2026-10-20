use std::sync::Mutex;

use providers::ContentBlock;

use super::*;
use crate::source::tests::Recorded;
use crate::test_support::{ScriptedModel, Step, decision, text, tool};

struct Fixture {
    recorded: Recorded,
    user: tempfile::TempDir,
    agents: Arc<Mutex<Vec<Arc<ScriptedModel>>>>,
}

impl Fixture {
    async fn new() -> Self {
        let recorded = Recorded::new();
        let project = recorded.source.root().canonicalize().unwrap();
        recorded.conversation("thread-1", &project).await;
        Self {
            recorded,
            user: tempfile::tempdir().unwrap(),
            agents: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn args(&self, candidate: Option<&str>) -> EvalArgs {
        let candidate = candidate.map(|text| {
            let path = self.user.path().join("candidate.toml");
            std::fs::write(&path, text).unwrap();
            path
        });
        EvalArgs {
            thread_id: "thread-1".into(),
            candidate,
            repeat: Some(1),
            output: Some(self.user.path().join("out")),
            db: Some(self.recorded.config.db_path.clone()),
            user_config_dir: Some(self.user.path().to_path_buf()),
        }
    }

    fn deps(&self, agent: Vec<Step>, user: Vec<Step>) -> RunnerDeps {
        let agents = Arc::clone(&self.agents);
        let credentials =
            sandbox::credential::FileCredentialStore::open(self.user.path().join("credentials"))
                .unwrap();
        RunnerDeps {
            env: Arc::new(routing::MapEnv::default()),
            isolation: Isolation::DirectUnchecked,
            models: Some(FixedModels {
                agent: Arc::new(move |bus| {
                    let model = ScriptedModel::new(agent.clone());
                    model.attach(bus);
                    agents.lock().unwrap().push(Arc::clone(&model));
                    model
                }),
                simulator: Arc::new(move |bus| {
                    let model = ScriptedModel::new(user.clone());
                    model.attach(bus);
                    model
                }),
            }),
            credentials: Some(Arc::new(credentials)),
            catalog: None,
        }
    }

    fn system_prompts(&self) -> Vec<String> {
        self.agents
            .lock()
            .unwrap()
            .iter()
            .map(|model| {
                let requests = model.requests();
                let system = requests[0]
                    .iter()
                    .find(|message| message.role == providers::Role::System)
                    .expect("system prompt");
                system
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect()
            })
            .collect()
    }
}

fn read_json(path: &Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

#[tokio::test]
async fn baseline_and_candidate_replays_each_leave_their_results() {
    // Given: a recorded thread, an agent that writes a file, and a user who then finishes.
    let fixture = Fixture::new().await;
    let overlay = "[sandbox]\nweb_tools_enabled = false\n";
    let deps = fixture.deps(
        vec![
            tool(
                "write",
                serde_json::json!({"path": "notes.txt", "content": "replayed\n"}),
            ),
            text("wrote the notes"),
        ],
        vec![decision("finish", "")],
    );
    let mut reported = Vec::new();

    // When: the evaluation runs once per configuration.
    let evaluation = run(fixture.args(Some(overlay)), deps, |report| {
        reported.push(format!("{}-{}", report.label, report.index));
    })
    .await
    .unwrap();

    // Then: both replays completed, in order, each with its own files and nonce.
    assert_eq!(reported, ["baseline-1", "candidate-1"]);
    let manifest = read_json(&evaluation.dir.join(output::MANIFEST_FILE));
    assert_eq!(manifest["thread_id"], "thread-1");
    assert_eq!(manifest["candidate_overlay"], overlay);
    assert_eq!(
        manifest["configurations"],
        serde_json::json!([BASELINE, CANDIDATE])
    );
    assert!(
        manifest["simulator"]["source"]
            .as_str()
            .unwrap()
            .contains("unset")
    );
    assert!(manifest["finished_at"].is_string());
    let nonces: Vec<&str> = manifest["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|run| run["nonce"].as_str().unwrap())
        .collect();
    assert_eq!(nonces.len(), 2);
    assert_ne!(nonces[0], nonces[1]);
    let prompts = fixture.system_prompts();
    for (prompt, nonce) in prompts.iter().zip(&nonces) {
        assert!(prompt.contains(&format!("Run identifier: {nonce}")));
    }
    for report in &evaluation.runs {
        assert_eq!(
            report.outcome.status,
            Status::Completed,
            "{}",
            report.outcome.reason
        );
        let outcome = read_json(&report.dir.join(output::OUTCOME_FILE));
        assert_eq!(outcome["status"], "completed");
        let conversation = read_json(&report.dir.join(output::CONVERSATION_FILE));
        assert_eq!(
            conversation[0],
            serde_json::json!({"kind": "user", "text": "fix the bug"})
        );
        let diff = std::fs::read_to_string(report.dir.join(output::DIFF_FILE)).unwrap();
        assert!(
            diff.contains("b/notes.txt") && diff.contains("+replayed"),
            "{diff}"
        );
        let database =
            storage::Database::open_read_only(&report.dir.join(output::DATABASE_FILE)).unwrap();
        assert!(database.thread_start_state("thread-1").unwrap().is_some());
        let runs = database.persisted_run_ids().unwrap();
        let [root] = runs.as_slice() else {
            panic!("one replay run: {runs:?}");
        };
        let inspected = database.inspect_run(root, false).unwrap();
        assert!(inspected["context"].is_object(), "{inspected}");
        assert!(
            !inspected["usage_requests"].as_array().unwrap().is_empty(),
            "{inspected}"
        );
    }
}

#[tokio::test]
async fn unpreparable_workspace_is_recorded_as_prepare_failed() {
    // Given: a recorded thread whose source repository has since lost its history.
    let fixture = Fixture::new().await;
    std::fs::remove_dir_all(fixture.recorded.source.root().join(".git")).unwrap();

    // When: the baseline is replayed.
    let evaluation = run(
        fixture.args(None),
        fixture.deps(Vec::new(), Vec::new()),
        |_| {},
    )
    .await
    .unwrap();

    // Then: the replay never started and its outcome says why.
    let [report] = evaluation.runs.as_slice() else {
        panic!("one replay");
    };
    assert_eq!(report.outcome.status, Status::PrepareFailed);
    assert_eq!(
        read_json(&report.dir.join(output::OUTCOME_FILE))["status"],
        "prepare_failed"
    );
    assert!(!report.dir.join(output::DATABASE_FILE).exists());
    assert!(fixture.agents.lock().unwrap().is_empty());
}

#[tokio::test]
async fn unusable_overlays_and_used_output_directories_are_refused_before_replaying() {
    // Given: an overlay that changes trusted tools, and an output directory already in use.
    let fixture = Fixture::new().await;
    let tools = fixture.args(Some("[tools.browser]\nenabled = true\n"));
    let used = fixture.args(None);
    let output = used.output.clone().unwrap();
    std::fs::create_dir_all(&output).unwrap();
    std::fs::write(output.join("previous.txt"), "keep").unwrap();

    // When: each evaluation is started.
    let tools = run(tools, fixture.deps(Vec::new(), Vec::new()), |_| {}).await;
    let used = run(used, fixture.deps(Vec::new(), Vec::new()), |_| {}).await;

    // Then: both are refused and nothing was replayed or overwritten.
    assert!(matches!(tools, Err(EvalError::Overlay(_))), "{tools:?}");
    assert!(matches!(used, Err(EvalError::Io(_))), "{used:?}");
    assert!(!output.join("runs").exists());
    assert_eq!(
        std::fs::read_to_string(output.join("previous.txt")).unwrap(),
        "keep"
    );
    assert!(fixture.agents.lock().unwrap().is_empty());
}

#[test]
fn simulator_uses_its_binding_or_reports_the_worker_fallback() {
    // Given: a configuration without and with an `eval.user_simulator` binding.
    let base = Config::default();
    let mut bound = Config::default();
    bound.eval.user_simulator = Some(config::RoleBindingConfig {
        preset: Some("simulator".into()),
        ..config::RoleBindingConfig::default()
    });

    // When: the simulator configuration is derived.
    let (fallback, fallback_summary) = simulator_config(&base);
    let (config, summary) = simulator_config(&bound);

    // Then: the binding takes the worker slot, or the worker binding is named as the source.
    assert_eq!(fallback.agents.worker.base, base.agents.worker.base);
    assert!(fallback_summary.source.contains("unset"));
    assert_eq!(
        config.agents.worker.base.preset.as_deref(),
        Some("simulator")
    );
    assert_eq!(summary.source, "eval.user_simulator");
}
